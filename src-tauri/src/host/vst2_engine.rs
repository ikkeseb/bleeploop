//! OWNS: a 64-bit VST2 plugin in the engine: `Vst2Unit` (the effect's process call with its rows,
//! pointer arrays, event list and time info, as an `lf_engine::SlotProcessor`) and the engine-mode
//! owner thread that loads, resumes, restarts, re-activates after an eviction and tears it down
//! (`engine_slot` holds the API), keeping the plugin's tone as `engine_slot` describes: its bank chunk
//! or its parameter values in one container (`save_state`, `restore_state`). The interface, the
//! loader and the host callback are `host/vst2.rs`.
//!
//! What differs from the other two formats, and must stay so:
//! - The plugin calls the host from any thread and leaves latches in its `HostContext`; the owner
//!   drains them every turn. Nothing the plugin reports is echoed back to it.
//! - `effStartProcess`/`effStopProcess` and `effMainsChanged` run on the OWNER; the unit's `stop`
//!   calls nothing. The only dispatcher opcode the audio thread sends is `effProcessEvents`.
//! - After `audioMasterIOChanged` the unit makes no plugin call at all until the owner has cycled
//!   the plugin (`HostContext::halted`).
//! - A dispatcher's return is its opcode's own: 0 from `effOpen`, `effMainsChanged`,
//!   `effStartProcess`, `effStopProcess` or `effSetChunk` is no failure.
//! - After `effClose` the `AEffect` is freed memory. A unit the engine does not hand back keeps the
//!   effect, its context and its module loaded with it (`teardown`).

use super::engine_slot::{
    keep_tone, report_faults, restore_tone, EngineSlotEvent, OwnerCtx, Ready, FAULT_EVENTS, FAULT_PARAM,
    OWNER_POLL, REMOVE_TIMEOUT,
};
use super::super::editor_window::{
    create_host_window, drain_after_editor_teardown, pump_thread_messages, set_client_size,
    show_host_window_front, wait_for_input, HostWindow,
};
use super::super::state::{ParamDesc, ToneRestore};
use super::super::tone::{self, ToneKeeper, Vst2State, VST2_MAX_CHUNK};
use super::super::vst2::{
    open_effect, time_info, validate, EffectInfo, HostContext, OpenError, ProcessingScope, Vst2Effect, Vst2Module,
};
use super::super::vst2_abi::*;
use super::{publish_param_ids, take_uncancelled, OwnerRequest, PluginEvent, MAX_EVENTS_PER_BLOCK, MAX_PLUGIN_CHANNELS};

use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{
    AtomicU32,
    Ordering::{Acquire, Relaxed},
};
use std::sync::Arc;
use std::time::Instant;

use lf_engine::grid::Frame;
use lf_engine::slots::MAX_SLOT_EVENTS;
use lf_engine::{SlotEvent, SlotEventKind, SlotKind, SlotProcessor};
use rtrb::Consumer;
use windows::Win32::Foundation::HWND;

use crate::engine_io::SlotHost;

/// Entries in each pointer array a process call gets: the most pins `vst2::validate` accepts. The
/// arrays ALWAYS hold this many, whatever the plugin declared.
const PINS: usize = MAX_PLUGIN_CHANNELS as usize;

/// The editor's size until the plugin reports one (the other formats' fallback).
const PROVISIONAL_EDITOR: (u32, u32) = (900, 600);

/// One resumed VST2 effect's process call with everything it touches, built on the owner thread.
pub(super) struct Vst2Unit {
    effect: *mut AEffect,
    ctx: Arc<HostContext>,
    dispatcher: DispatcherFn,
    process: ProcessFn,
    /// `process` overwrites its outputs (`processReplacing`); else it adds into them.
    replacing: bool,
    set_parameter: Option<SetParameterFn>,
    param_count: usize,
    kind: SlotKind,
    latency: Frame,
    max_frames: usize,
    params: Consumer<PluginEvent>,
    /// One `max_frames` row per declared input and output.
    in_rows: Vec<Vec<f32>>,
    out_rows: Vec<Vec<f32>>,
    /// What every pointer entry past the declared pins points at: inputs read silence, outputs are
    /// written where nothing reads. A plugin that grows its pin count inside the bound therefore
    /// stays inside memory this unit owns, even before the owner has reacted.
    silent: Vec<f32>,
    discard: Vec<f32>,
    in_ptrs: [*mut f32; PINS],
    out_ptrs: [*mut f32; PINS],
    /// The slice's notes as the plugin reads them: the list points into `midi`.
    events: Box<VstEvents<MAX_SLOT_EVENTS>>,
    midi: Box<[VstMidiEvent; MAX_SLOT_EVENTS]>,
    /// What `audioMasterGetTime` hands the plugin from inside a call this unit makes.
    time: Box<UnsafeCell<VstTimeInfo>>,
    /// Frames processed since the unit was built.
    position: u64,
    faults: Arc<AtomicU32>,
}

// SAFETY: one thread at a time holds the unit. The owner builds it and hands it over; the engine
// touches it only under its lock (the device callback, or whoever holds the engine while no device
// runs); it comes back through the slot's port before the owner touches it again. The row, event
// and time pointers point only into the unit itself, the plugin sees them only during a call the
// unit makes, and the `AEffect` stays alive for as long as the unit does: its owner closes it only
// with the unit back in hand, and leaks it otherwise.
unsafe impl Send for Vst2Unit {}

impl Vst2Unit {
    /// # Safety
    /// `effect` is the open effect `info` was validated from, bound to `ctx`, and it stays open for
    /// as long as the unit may process.
    unsafe fn new(
        effect: *mut AEffect,
        ctx: Arc<HostContext>,
        info: &EffectInfo,
        max_frames: usize,
        params: Consumer<PluginEvent>,
        faults: Arc<AtomicU32>,
    ) -> Box<Vst2Unit> {
        let mut unit = Box::new(Vst2Unit {
            effect,
            ctx,
            dispatcher: info.dispatcher,
            process: info.process,
            replacing: info.replacing,
            set_parameter: info.set_parameter,
            param_count: info.params,
            kind: SlotKind::Instrument,
            latency: 0,
            max_frames: 0,
            params,
            in_rows: Vec::new(),
            out_rows: Vec::new(),
            silent: Vec::new(),
            discard: Vec::new(),
            in_ptrs: [std::ptr::null_mut(); PINS],
            out_ptrs: [std::ptr::null_mut(); PINS],
            events: Box::new(VstEvents { num_events: 0, reserved: 0, events: [std::ptr::null_mut(); MAX_SLOT_EVENTS] }),
            midi: Box::new([VstMidiEvent::default(); MAX_SLOT_EVENTS]),
            time: Box::new(UnsafeCell::new(time_info(0.0, 0.0))),
            position: 0,
            faults,
        });
        unit.rearm(info, max_frames);
        unit
    }

    /// Owner thread: size the rows to what the plugin declares now (a restart can change the pin
    /// counts, a device change the block) and take the calls it was validated with.
    fn rearm(&mut self, info: &EffectInfo, max_frames: usize) {
        self.dispatcher = info.dispatcher;
        self.process = info.process;
        self.replacing = info.replacing;
        self.set_parameter = info.set_parameter;
        self.param_count = info.params;
        self.kind = if info.is_synth() || info.inputs == 0 { SlotKind::Instrument } else { SlotKind::Effect };
        self.latency = Frame::from(info.initial_delay);
        self.max_frames = max_frames.max(1);
        self.in_rows = vec![vec![0.0; self.max_frames]; info.inputs];
        self.out_rows = vec![vec![0.0; self.max_frames]; info.outputs.max(1)];
        self.silent = vec![0.0; self.max_frames];
        self.discard = vec![0.0; self.max_frames];
    }

    /// Point all `PINS` entries of both arrays at this unit's rows: each declared pin at its own,
    /// the rest at the scratch rows.
    fn point_rows(&mut self) {
        self.in_ptrs = [self.silent.as_mut_ptr(); PINS];
        self.out_ptrs = [self.discard.as_mut_ptr(); PINS];
        for (entry, row) in self.in_ptrs.iter_mut().zip(self.in_rows.iter_mut()) {
            *entry = row.as_mut_ptr();
        }
        for (entry, row) in self.out_ptrs.iter_mut().zip(self.out_rows.iter_mut()) {
            *entry = row.as_mut_ptr();
        }
    }

    /// The ring's parameter edits, at the block start: more than the cap wait in the ring. An index
    /// the plugin never declared and a value that is no number are dropped and latched.
    fn drain_params(&mut self) {
        for _ in 0..MAX_EVENTS_PER_BLOCK {
            // `setParameter` may itself report a layout change: nothing is sent after that.
            if self.ctx.halted() {
                return;
            }
            let Ok(event) = self.params.pop() else { return };
            // Only params ride this ring in engine mode: the engine routes the notes.
            let PluginEvent::Param { id, value } = event else { continue };
            let value = value as f32;
            match self.set_parameter.filter(|_| (id as usize) < self.param_count && value.is_finite()) {
                // SAFETY: the open effect's own `setParameter`, with an index it declared, on the
                // thread that processes it (where a VST2 host sets a parameter).
                Some(set_parameter) => unsafe { set_parameter(self.effect, id as i32, value.clamp(0.0, 1.0)) },
                None => {
                    self.faults.fetch_or(FAULT_PARAM, Relaxed);
                }
            }
        }
    }

    /// Fill the event list with the notes of the slice `[at, at + len)` and return how many: note-on
    /// and note-off on channel 0, each at its offset inside the slice. The list holds what the
    /// engine queues between two renders (`MAX_SLOT_EVENTS`), so a note past it is a broken caller:
    /// it is DROPPED and latched, never carried into a later slice, where it would sound late at an
    /// offset that is no longer its own.
    fn collect_notes(&mut self, events: &[SlotEvent], next: &mut usize, at: usize, len: usize) -> usize {
        let mut count = 0;
        while let Some(e) = events.get(*next).filter(|e| (e.offset as usize) < at + len) {
            *next += 1;
            if count == MAX_SLOT_EVENTS {
                self.faults.fetch_or(FAULT_EVENTS, Relaxed);
                continue;
            }
            let midi_data = match e.kind {
                SlotEventKind::NoteOn { key, velocity } => {
                    let velocity = if velocity.is_finite() { velocity.clamp(0.0, 1.0) } else { 0.0 };
                    // A note-on with velocity 0 is a note-off: the quietest note is 1.
                    [0x90, key & 0x7f, ((velocity * 127.0).round() as u8).clamp(1, 127), 0]
                }
                SlotEventKind::NoteOff { key } => [0x80, key & 0x7f, 0, 0],
            };
            self.midi[count] = VstMidiEvent {
                event_type: VST_MIDI_TYPE,
                byte_size: std::mem::size_of::<VstMidiEvent>() as i32,
                delta_frames: (e.offset as usize).saturating_sub(at) as i32,
                midi_data,
                ..VstMidiEvent::default()
            };
            count += 1;
        }
        for (entry, event) in self.events.events.iter_mut().zip(self.midi.iter_mut()).take(count) {
            *entry = event;
        }
        self.events.num_events = count as i32;
        count
    }
}

impl SlotProcessor for Vst2Unit {
    fn kind(&self) -> SlotKind {
        self.kind
    }

    fn latency(&self) -> Frame {
        self.latency
    }

    fn process(&mut self, _frame: Frame, input: &[f32], events: &[SlotEvent], out: &mut [f32]) {
        out.fill(0.0);
        // The plugin's layout is no longer the one these rows were built for: silence, and no call,
        // until the owner has cycled it and rebuilt them.
        if self.ctx.halted() {
            return;
        }
        let rate = self.ctx.sample_rate();
        // SAFETY: the time info is the unit's own, written by this thread alone, and outlives the
        // scope, which ends with this call.
        let _processing = unsafe {
            *self.time.get() = time_info(rate, self.position as f64);
            ProcessingScope::enter(&self.ctx, self.time.get())
        };
        self.drain_params();
        let n = out.len();
        let (mut at, mut next) = (0, 0);
        // A call longer than the plugin's block size goes in slices; each note lands in its own.
        while at < n {
            if self.ctx.halted() {
                return;
            }
            let len = (n - at).min(self.max_frames);
            // SAFETY: as above; the plugin reads it only from inside the calls below.
            unsafe { *self.time.get() = time_info(rate, self.position as f64) };
            if self.collect_notes(events, &mut next, at, len) > 0 {
                let list: *mut VstEvents<MAX_SLOT_EVENTS> = &mut *self.events;
                // SAFETY: the open effect's dispatcher; the list and the events it points at are
                // the unit's own and stay as they are for the call (`num_events` of them are set).
                unsafe { (self.dispatcher)(self.effect, EFF_PROCESS_EVENTS, 0, 0, list.cast(), 0.0) };
                if self.ctx.halted() {
                    return;
                }
            }
            // The mono input feeds the first two inputs; any other declared input stays silent.
            for row in self.in_rows.iter_mut().take(2) {
                row[..len].copy_from_slice(&input[at..at + len]);
            }
            // The accumulating call adds into every output; the replacing one may leave the pair
            // this unit reads untouched while it has nothing to say.
            let cleared = if self.replacing { 2 } else { self.out_rows.len() };
            for row in self.out_rows.iter_mut().take(cleared) {
                row[..len].fill(0.0);
            }
            self.point_rows();
            // SAFETY: the open effect's process call. Both arrays hold `PINS` valid rows of at
            // least `len` frames (`len` ≤ `max_frames`), all owned by this unit for the call.
            unsafe { (self.process)(self.effect, self.in_ptrs.as_mut_ptr(), self.out_ptrs.as_mut_ptr(), len as i32) };
            // The first stereo pair is the slot's sound; a multi-output instrument's other pins are
            // auxiliaries, and averaging them in would only dilute it.
            let mono = &mut out[at..at + len];
            match self.out_rows.as_slice() {
                [left, right, ..] => {
                    for (sample, (l, r)) in mono.iter_mut().zip(left.iter().zip(right.iter())) {
                        *sample = (l + r) * 0.5;
                    }
                }
                [only] => mono.copy_from_slice(&only[..len]),
                [] => {}
            }
            self.position = self.position.wrapping_add(len as u64);
            self.ctx.set_sample_pos(self.position);
            at += len;
        }
    }

    /// Bookkeeping only: `effStopProcess` is the owner's call (`Vst2Plugin::suspend`).
    fn stop(&mut self) {}

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
        self
    }
}

/// A unit the engine handed back: a `SlotHost` hands back only what its own handle installed.
fn own(unit: Box<dyn SlotProcessor>) -> Box<Vst2Unit> {
    unit.into_any()
        .downcast::<Vst2Unit>()
        .unwrap_or_else(|_| panic!("an engine slot hands back the unit its owner installed"))
}

/// Everything the owner holds of a loaded plugin besides the unit. Not `Send` (`Vst2Effect`): every
/// call below runs on the owner thread. Dropping it closes the plugin in the live order: stopped
/// and suspended, the editor closed, `effClose`, then the fields in declaration order (the context
/// after `effClose`, the module LAST). The unit must be out of the engine first; when it is not,
/// the plugin is forgotten instead (`teardown`).
struct Vst2Plugin {
    effect: Vst2Effect,
    /// What the effect declared at its last activation (`resume`).
    info: EffectInfo,
    /// Between `effMainsChanged(1)` + `effStartProcess` and their counterparts.
    running: bool,
    /// The open editor's host window.
    editor: Option<HostWindow>,
    ctx: Arc<HostContext>,
    /// `None` for an in-process plugin (the fixture). Held only to unload last.
    _module: Option<Vst2Module>,
}

impl Drop for Vst2Plugin {
    fn drop(&mut self) {
        self.suspend();
        self.close_editor();
        // SAFETY: owner thread; the unit is out of the engine, so no thread is inside the plugin.
        // The effect is freed by this call and nothing reads it afterwards.
        unsafe { self.effect.dispatch(EFF_CLOSE, 0, 0, std::ptr::null_mut(), 0.0) };
    }
}

impl Vst2Plugin {
    /// A dispatcher call that passes no pointer.
    ///
    /// # Safety
    /// `opcode` is one that reads no `ptr`.
    unsafe fn call(&self, opcode: i32, index: i32, value: isize, opt: f32) -> isize {
        // SAFETY: owner thread (the plugin never leaves it); the caller's contract.
        unsafe { self.effect.dispatch(opcode, index, value, std::ptr::null_mut(), opt) }
    }

    /// Give the plugin the engine's current rate and largest block, the context first (the plugin
    /// may ask the host from inside either call). Plugin suspended.
    fn configure(&mut self, slot: &SlotHost) -> Result<(u32, usize), String> {
        let rate = slot.rate().ok_or_else(|| "no audio device is open".to_string())?;
        let block = slot.max_block().max(1);
        self.ctx.set_rate(f64::from(rate), block as i32)?;
        // SAFETY: neither opcode reads a pointer.
        unsafe {
            self.call(EFF_SET_SAMPLE_RATE, 0, 0, rate as f32);
            self.call(EFF_SET_BLOCK_SIZE, 0, block as isize, 0.0);
        }
        Ok((rate, block))
    }

    /// `effMainsChanged(1)` then `effStartProcess`, and what the effect declares once it runs. Its
    /// declaration is checked on both sides: before, so a layout this host refuses is never
    /// resumed, and after, because a plugin settles its pins and its latency inside these calls
    /// (and says so with `audioMasterIOChanged`, which then describes the state just read and is
    /// consumed). A refusal leaves the plugin suspended.
    fn resume(&mut self) -> Result<EffectInfo, String> {
        // SAFETY: the open effect's own structure, read on its owner thread while it is suspended.
        unsafe { validate(self.effect.raw()) }?;
        self.ctx.clear_halted();
        // SAFETY: neither opcode reads a pointer. Their returns carry no verdict.
        unsafe {
            self.call(EFF_MAINS_CHANGED, 0, 1, 0.0);
            self.call(EFF_START_PROCESS, 0, 0, 0.0);
        }
        self.running = true;
        // SAFETY: as above; nothing processes the effect yet (its unit is out of the engine).
        match unsafe { validate(self.effect.raw()) } {
            Ok(info) => {
                let _ = self.ctx.take_restart();
                self.ctx.clear_halted();
                self.info = info;
                Ok(info)
            }
            Err(e) => {
                self.suspend();
                Err(e)
            }
        }
    }

    /// `effStopProcess` then `effMainsChanged(0)`, on a running plugin only. The unit is out of the
    /// engine, or is about to be leaked with the plugin still loaded (never reached then).
    fn suspend(&mut self) {
        if std::mem::take(&mut self.running) {
            // SAFETY: neither opcode reads a pointer.
            unsafe {
                self.call(EFF_STOP_PROCESS, 0, 0, 0.0);
                self.call(EFF_MAINS_CHANGED, 0, 0, 0.0);
            }
        }
    }

    /// The plugin's parameters: ids are indices, values normalised 0..1. VST2 names no default, so
    /// the live value stands in for it.
    fn list_params(&self) -> Vec<ParamDesc> {
        let Some(get_parameter) = self.info.get_parameter else { return Vec::new() };
        (0..self.info.params)
            .map(|index| {
                // SAFETY: owner thread; an index the effect declared. The name is read through a
                // buffer far larger than the 8 bytes the format promises (`Vst2Effect::string`).
                let (name, value) = unsafe {
                    (self.effect.string(EFF_GET_PARAM_NAME, index as i32), get_parameter(self.effect.raw(), index as i32))
                };
                let value = if value.is_finite() { f64::from(value.clamp(0.0, 1.0)) } else { 0.0 };
                ParamDesc {
                    id: index as u32,
                    name: name.unwrap_or_else(|| format!("Parameter {}", index + 1)),
                    min_value: 0.0,
                    max_value: 1.0,
                    default_value: value,
                    value,
                }
            })
            .collect()
    }

    /// The current program's index, 0 when the plugin answers one it does not have.
    fn program(&self) -> i32 {
        // SAFETY: the opcode reads no pointer.
        let program = unsafe { self.call(EFF_GET_PROGRAM, 0, 0, 0.0) };
        i32::try_from(program).ok().filter(|p| (0..self.info.programs as i64).contains(&i64::from(*p))).unwrap_or(0)
    }

    /// The plugin's state, for its tone (`tone::encode_vst2`): its bank chunk when it keeps one
    /// (`effFlagsProgramChunks`), else every parameter's value. The chunk is the plugin's own
    /// memory, returned with a signed length: both are checked before a slice is made of them, the
    /// bytes are copied at once, with no call into the plugin in between, and never freed. A length
    /// of 0 or less is a plugin with nothing to save. The unit keeps processing meanwhile.
    fn save_state(&self) -> Result<Option<Vec<u8>>, String> {
        let program = self.program();
        if !self.info.program_chunks() {
            let values = match self.info.get_parameter {
                // SAFETY: owner thread; indices the effect declared.
                Some(get_parameter) => {
                    (0..self.info.params).map(|i| unsafe { get_parameter(self.effect.raw(), i as i32) }).collect()
                }
                None => Vec::new(),
            };
            return Ok(Some(tone::encode_vst2(&Vst2State::Params { program, values })));
        }
        let mut data: *mut c_void = std::ptr::null_mut();
        // SAFETY: owner thread; `effGetChunk` (index 0: the bank) writes one pointer through `ptr`.
        let len = unsafe { self.effect.dispatch(EFF_GET_CHUNK, 0, 0, (&raw mut data).cast(), 0.0) };
        if len <= 0 {
            return Ok(None);
        }
        if len as usize > VST2_MAX_CHUNK {
            return Err(format!("the plugin's chunk is {len} bytes, more than {VST2_MAX_CHUNK}"));
        }
        if data.is_null() {
            return Err(format!("the plugin answered a {len}-byte chunk with no pointer"));
        }
        // SAFETY: the plugin answered `len` readable bytes at `data` (a length in 1..=the cap), and
        // they stay as they are until its next call, which comes only after this copy.
        let chunk = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), len as usize) }.to_vec();
        Ok(Some(tone::encode_vst2(&Vst2State::Chunk { program, chunk })))
    }

    /// Restore a tone's state, before `resume`. A chunk goes to `effSetChunk` and no program is
    /// selected after it: a bank chunk restores its own. Parameters go after their program
    /// (`effSetProgram` can replace every parameter, so the other order would lose them), each
    /// finite value clamped to 0..1. Everything is checked against the plugin before its first
    /// call, so a state this refuses left the instance untouched, at its defaults.
    fn restore_state(&self, state: &[u8]) -> Result<(), String> {
        match tone::decode_vst2(state)? {
            Vst2State::Chunk { chunk, .. } => {
                if !self.info.program_chunks() {
                    return Err("the tone holds a chunk and the plugin takes none".to_string());
                }
                if chunk.is_empty() {
                    return Err("the tone holds an empty chunk".to_string());
                }
                // SAFETY: owner thread; `effSetChunk` (index 0: the bank) reads `value` bytes at
                // `ptr`, which outlive the call. Its return carries no verdict.
                unsafe { self.effect.dispatch(EFF_SET_CHUNK, 0, chunk.len() as isize, chunk.as_ptr() as *mut c_void, 0.0) };
            }
            Vst2State::Params { program, values } => {
                let (params, programs) = (self.info.params, self.info.programs);
                if values.len() != params {
                    return Err(format!("the tone holds {} parameters and the plugin has {params}", values.len()));
                }
                let known = usize::try_from(program).is_ok_and(|p| p < programs) || (program == 0 && programs == 0);
                if !known {
                    return Err(format!("the tone names program {program} and the plugin has {programs}"));
                }
                if programs > 0 {
                    // SAFETY: the opcode reads no pointer; the index is one the effect declared.
                    unsafe { self.call(EFF_SET_PROGRAM, 0, program as isize, 0.0) };
                }
                if let Some(set_parameter) = self.info.set_parameter {
                    for (index, value) in values.into_iter().enumerate().filter(|(_, v)| v.is_finite()) {
                        // SAFETY: owner thread, before the plugin processes; a declared index.
                        unsafe { set_parameter(self.effect.raw(), index as i32, value.clamp(0.0, 1.0)) };
                    }
                }
            }
        }
        Ok(())
    }

    /// The size the plugin's editor reports (`effEditGetRect`). The plugin answers a pointer to a
    /// rectangle of its own, which is copied at once and trusted for nothing: `None` when there is
    /// none or it is empty.
    fn editor_rect(&self) -> Option<(u32, u32)> {
        let mut rect: *mut ERect = std::ptr::null_mut();
        // SAFETY: owner thread; `effEditGetRect` writes one pointer through `ptr`.
        unsafe { self.effect.dispatch(EFF_EDIT_GET_RECT, 0, 0, (&raw mut rect).cast(), 0.0) };
        if rect.is_null() {
            return None;
        }
        // SAFETY: a non-null answer points at the plugin's `ERect`; read once, unaligned or not.
        let rect = unsafe { rect.read_unaligned() };
        let width = i32::from(rect.right) - i32::from(rect.left);
        let height = i32::from(rect.bottom) - i32::from(rect.top);
        (width > 0 && height > 0).then_some((width as u32, height as u32))
    }

    /// Open the plugin's editor inside a host window of its own, still hidden: the caller shows it
    /// (`show_host_window_front`). The window exists, and the context knows it, before
    /// `effEditOpen`, so a plugin that asks for a size from inside it is answered at once; the size
    /// it reports once it is open (many know it only then) replaces the one it gave before. A
    /// plugin that refuses (`effEditOpen` answers 0) is told to close and its window is destroyed.
    fn embed_editor(&mut self, parent: usize) -> Result<HWND, String> {
        if let Some(open) = &self.editor {
            return Ok(open.hwnd);
        }
        if !self.info.has_editor() {
            return Err("plugin has no editor".to_string());
        }
        let (width, height) = self.editor_rect().unwrap_or(PROVISIONAL_EDITOR);
        let parent = (parent != 0).then_some(HWND(parent as *mut c_void));
        let window = create_host_window(width, height, parent)?;
        let hwnd = window.hwnd;
        self.ctx.set_editor_window(hwnd.0 as isize);
        self.editor = Some(window);
        // SAFETY: owner thread; `effEditOpen` takes the parent window's handle in `ptr`, and the
        // window lives until `close_editor` has told the plugin to leave it.
        if unsafe { self.effect.dispatch(EFF_EDIT_OPEN, 0, 0, hwnd.0, 0.0) } == 0 {
            self.close_editor();
            return Err("the plugin refused to open its editor".to_string());
        }
        if let Some((width, height)) = self.editor_rect() {
            let _ = set_client_size(hwnd, width, height);
        }
        // A size the plugin asked for from another thread while it opened.
        if let Some((width, height)) = self.ctx.take_resize() {
            let _ = set_client_size(hwnd, width as u32, height as u32);
        }
        Ok(hwnd)
    }

    /// Close the editor, if one is open: `effEditClose` while the window still exists, then the
    /// window, then its teardown's messages. True when there was one.
    fn close_editor(&mut self) -> bool {
        let Some(window) = self.editor.take() else { return false };
        // SAFETY: the opcode reads no pointer; the plugin's child leaves the window before it goes.
        unsafe { self.call(EFF_EDIT_CLOSE, 0, 0, 0.0) };
        self.ctx.set_editor_window(0);
        drop(window);
        drain_after_editor_teardown();
        true
    }
}

/// Re-activate the plugin at the engine's current terms and install its unit, rows rebuilt. On
/// failure the unit comes back to the caller and the plugin is left suspended.
fn reinstall(plugin: &mut Vst2Plugin, mut unit: Box<Vst2Unit>, slot: &SlotHost) -> Result<(), (Box<Vst2Unit>, String)> {
    plugin.suspend();
    let activated = plugin.configure(slot).and_then(|terms| Ok((plugin.resume()?, terms)));
    match activated {
        Ok((info, (rate, block))) => {
            unit.rearm(&info, block);
            slot.install(unit, rate).map_err(|(unit, e)| (own(unit), e))
        }
        Err(e) => Err((unit, e)),
    }
}

/// A restart or an eviction: get the unit back (it came from the engine, the owner still holds it
/// from a failed cycle, or the engine hands it back now) and reinstall it.
fn cycle(
    plugin: &mut Vst2Plugin,
    slot: &SlotHost,
    parked: &mut Option<Box<Vst2Unit>>,
    back: Option<Box<Vst2Unit>>,
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
    match reinstall(plugin, unit, slot) {
        Ok(()) => log::info!("[plugin_host] engine slot {index} {why}: stopped → suspended → resumed → reinstalled"),
        Err((unit, e)) => {
            log::error!("[plugin_host] engine slot {index} {why} failed, the slot stays bypassed: {e}");
            *parked = Some(unit);
        }
    }
}

/// What an owner loads from: the module (`None` for an in-process plugin), its entry, and the
/// name a plugin that names nothing gets (the scan's: the file's stem).
pub(super) struct Opened {
    pub(super) module: Option<Vst2Module>,
    pub(super) entry: EntryFn,
    pub(super) file_stem: String,
}

/// The production owner: load the DLL at `path`.
pub(super) fn run(ctx: OwnerCtx, path: String, id: &str) -> Result<(), String> {
    run_with(ctx, id, move || {
        let path = std::path::Path::new(&path);
        let module = Vst2Module::load(path)?;
        let entry = module.entry();
        let file_stem = path.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default();
        Ok(Opened { module: Some(module), entry, file_stem })
    })
}

/// What a load hands the owner: the plugin, its unit, its name, the rate it runs at and what it
/// did with the stored tone.
type Loaded = (Vst2Plugin, Box<Vst2Unit>, String, u32, Option<ToneRestore>);

/// Create the instance for `id` (the effect's unique id as 8 hex digits) on a context of its own,
/// bound to this thread as its owner. An instance this host refuses was never called, so it cannot
/// be closed: its module and its context stay loaded for the rest of the process.
fn create(module: Option<Vst2Module>, entry: EntryFn, id: &str, slot: &SlotHost) -> Result<Vst2Plugin, String> {
    let rate = slot.rate().ok_or_else(|| "no audio device is open".to_string())?;
    let ctx = HostContext::new(f64::from(rate), slot.max_block().max(1) as i32)?;
    ctx.bind_owner();
    // SAFETY: `entry` belongs to `module` (or to this process), which outlives the effect: the
    // plugin unloads it last, and every path that keeps the instance alive keeps both.
    let effect = match unsafe { open_effect(entry, &ctx) } {
        Ok(effect) => effect,
        Err(OpenError::NoInstance(e)) => return Err(e),
        Err(OpenError::Refused(e)) => {
            if let Some(module) = module {
                module.leak();
            }
            std::mem::forget(ctx);
            return Err(e);
        }
    };
    let info = *effect.info();
    // From here every early return drops `plugin`, which closes the effect before its module goes.
    let plugin = Vst2Plugin { effect, info, running: false, editor: None, ctx, _module: module };
    let found = format!("{:08x}", info.unique_id as u32);
    if found != id {
        return Err(format!("the file holds VST2 plugin {found}, not {id}"));
    }
    Ok(plugin)
}

/// Create the plugin, tell it the engine's rate and block, restore the stored tone, resume it and
/// build its unit. A tone this host cannot apply is refused before the plugin is called for it
/// (`restore_state`), so the instance that goes on is still at its defaults.
fn load(
    id: &str,
    open: impl FnOnce() -> Result<Opened, String>,
    slot: &SlotHost,
    params: Consumer<PluginEvent>,
    faults: Arc<AtomicU32>,
    tone: &mut ToneKeeper,
) -> Result<Loaded, String> {
    let Opened { module, entry, file_stem } = open()?;
    let mut plugin = create(module, entry, id, slot)?;
    // SAFETY: the thread that opened the effect.
    let name = unsafe { plugin.effect.name() }.unwrap_or(file_stem);
    let (rate, block) = plugin.configure(slot)?;
    // The stored tone goes in before the plugin resumes: nothing processes it yet.
    let restored = restore_tone(tone, slot.slot(), &name, |state| plugin.restore_state(state));
    let info = plugin.resume()?;
    // What the plugin reported while it opened, took its tone and resumed describes the state just
    // loaded: the parameters are listed after the load anyway.
    let _ = plugin.ctx.take_relist();
    if let Some(automation) = plugin.ctx.automation() {
        automation.drain(|_, _| {});
    }
    // SAFETY: the open effect `info` was just validated from, bound to this context; the plugin
    // closes it only with the unit back in hand.
    let unit = unsafe { Vst2Unit::new(plugin.effect.raw(), plugin.ctx.clone(), &info, block, params, faults) };
    Ok((plugin, unit, name, rate, restored.report()))
}

/// The engine-mode VST2 owner. `open` yields the module and its entry (a DLL in production, an
/// in-process entry in the tests); returns the teardown's result.
pub(super) fn run_with(ctx: OwnerCtx, id: &str, open: impl FnOnce() -> Result<Opened, String>) -> Result<(), String> {
    let OwnerCtx { slot, running, requests, params, params_tx: _, param_ids, sink, editor_parent, ready, mut tone } = ctx;
    let index = slot.slot();
    let faults = Arc::new(AtomicU32::new(0));
    let loaded = load(id, open, &slot, params, faults.clone(), &mut tone);
    let (mut plugin, unit, name, rate, restored) = match loaded {
        Ok(loaded) => loaded,
        Err(e) => {
            let _ = ready.send(Err(e));
            return Ok(());
        }
    };
    // Before the load is reported, so the caller's first `set_param` finds its ids.
    publish_param_ids(&param_ids, &plugin.list_params());
    let kind = unit.kind;
    // Not installed: the unit goes, then the plugin closes.
    if !running.load(Acquire) {
        drop(unit);
        drop(plugin);
        let _ = ready.send(Err("plugin load cancelled".to_string()));
        return Ok(());
    }
    if let Err((unit, e)) = slot.install(unit, rate) {
        drop(unit);
        drop(plugin);
        let _ = ready.send(Err(e));
        return Ok(());
    }
    if ready.send(Ok(Ready { name: name.clone(), kind, tone: restored })).is_err() {
        log::warn!("[plugin_host] engine slot {index}: the load finished after its caller gave up; undoing it");
        return teardown(&slot, plugin, None, None);
    }

    // The unit while it is out of the engine (a failed restart); `None` while the engine holds it.
    let mut parked: Option<Box<Vst2Unit>> = None;
    let mut reported = 0u32;
    // A panic in the loop must not unwind past a unit the engine still runs (dropping `plugin` would
    // close and unload code the audio thread is inside): it is caught here, and the ordered teardown
    // below takes the unit out of the engine first.
    let served = catch_unwind(AssertUnwindSafe(|| {
        while running.load(Acquire) {
            // audioMasterIOChanged only raised latches (on any thread, the audio thread included)
            // and silenced the unit; the cycle runs here.
            if plugin.ctx.take_restart() {
                cycle(&mut plugin, &slot, &mut parked, None, "audioMasterIOChanged");
            }
            // What the plugin's own editor moved, with the value the plugin reported: told to the
            // caller, kept for the tone, never read back from the plugin or sent to it again.
            let mut moved = false;
            if let Some(automation) = plugin.ctx.automation() {
                automation.drain(|param, value| {
                    moved = true;
                    sink(EngineSlotEvent::ParamChanged { id: param as u32, value: f64::from(value) });
                });
            }
            // After the cycle, so a re-list the plugin raised while it resumed lands this turn.
            if plugin.ctx.take_relist() {
                publish_param_ids(&param_ids, &plugin.list_params());
                sink(EngineSlotEvent::ParamsChanged);
                moved = true;
            }
            if moved {
                tone.note_change(Instant::now());
            }
            // A new engine at another rate evicted the unit; it comes back stopped.
            if let Some(unit) = slot.take_evicted() {
                cycle(&mut plugin, &slot, &mut parked, Some(own(unit)), "re-activation after a device change");
            }
            if plugin.ctx.take_need_idle() {
                // SAFETY: the opcode reads no pointer.
                unsafe { plugin.call(EFF_IDLE, 0, 0, 0.0) };
            }
            // Every turn, editor or not, as the other owners do: a plugin's own message-driven work
            // (timers, async updates) runs on this thread, and the hosted editor's window reports
            // its close box here.
            pump_thread_messages();
            if plugin.editor.is_some() {
                // SAFETY: the opcode reads no pointer.
                unsafe { plugin.call(EFF_EDIT_IDLE, 0, 0, 0.0) };
            }
            // A size the plugin asked for from a thread that may not touch the window. With no
            // editor open there is nothing to size, and the request is dropped.
            if let (Some((width, height)), Some(window)) = (plugin.ctx.take_resize(), &plugin.editor) {
                let _ = set_client_size(window.hwnd, width as u32, height as u32);
            }
            if plugin.editor.as_ref().is_some_and(HostWindow::close_requested) {
                plugin.close_editor();
                sink(EngineSlotEvent::EditorClosed);
                let _ = keep_tone(&mut tone, index, "editor closed", &name, || plugin.save_state());
            }
            let request = if plugin.editor.is_some() {
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
                        let was_closed = plugin.editor.is_none();
                        let res = if was_closed { plugin.embed_editor(editor_parent).map(show_host_window_front) } else { Ok(()) };
                        // Opened after the caller's 5 s: it reported failure, so close what this opened.
                        if was_closed && res.is_ok() && cancelled.load(Relaxed) {
                            plugin.close_editor();
                        }
                        let _ = reply.send(res);
                    }
                    OwnerRequest::CloseEditor(_, reply) => {
                        if plugin.close_editor() {
                            let _ = keep_tone(&mut tone, index, "editor closed", &name, || plugin.save_state());
                        }
                        let _ = reply.send(Ok(()));
                    }
                    OwnerRequest::SaveTone(reply) => {
                        let _ = reply.send(keep_tone(&mut tone, index, "asked", &name, || plugin.save_state()));
                    }
                    OwnerRequest::ListParams(reply) => {
                        let listed = plugin.list_params();
                        publish_param_ids(&param_ids, &listed);
                        let _ = reply.send(Ok(listed));
                    }
                    // VST3 only (`EngineSlotHandle::set_param`): a VST2 plugin is one object, and the
                    // unit's `setParameter` is all it needs.
                    OwnerRequest::SetParamNormalized(..) => {}
                    OwnerRequest::Wake => {}
                }
            }
            if tone.poll(Instant::now()) {
                let _ = keep_tone(&mut tone, index, "changes went quiet", &name, || plugin.save_state());
            }
            report_faults(&faults, index, &mut reported);
        }
    }));
    if served.is_err() {
        log::error!("[plugin_host] engine slot {index}: the VST2 owner panicked; tearing the plugin down");
    }
    // A change not saved yet, and what an open editor may have changed unseen. Not after a panic,
    // which may have left the plugin half way through something.
    let save = served.is_ok() && (plugin.editor.is_some() || tone.dirty());
    let result = teardown(&slot, plugin, parked, save.then_some((&mut tone, name.as_str())));
    report_faults(&faults, index, &mut reported);
    result
}

/// The ordered teardown: the unit leaves the engine → `effStopProcess` → `effMainsChanged(0)` → the
/// editor closes → the tone is saved (`save`: there is a change to keep) → `effClose` → the context
/// goes → the module unloads last. A unit the engine does not hand back is still inside the
/// plugin's code, and the plugin still calls the host: the unit, the effect, its context and its
/// module then stay loaded TOGETHER (leaked), and the plugin is neither stopped nor closed.
fn teardown(
    slot: &SlotHost,
    mut plugin: Vst2Plugin,
    parked: Option<Box<Vst2Unit>>,
    save: Option<(&mut ToneKeeper, &str)>,
) -> Result<(), String> {
    let index = slot.slot();
    let t = Instant::now();
    let unit = match parked {
        Some(unit) => Some(unit),
        None => match slot.remove(REMOVE_TIMEOUT) {
            Ok(unit) => unit.map(own),
            Err(e) => {
                log::error!("[plugin_host] engine slot {index} VST2 teardown: {e}; leaving the plugin loaded");
                // The editor is this thread's window and goes with it; the tone is saved as every
                // other save is, with the unit still processing.
                plugin.close_editor();
                if let Some((tone, name)) = save {
                    let _ = keep_tone(tone, index, "unload", name, || plugin.save_state());
                }
                slot.abandon();
                std::mem::forget(plugin);
                return Err(e);
            }
        },
    };
    let remove_ms = t.elapsed().as_millis();
    let t = Instant::now();
    plugin.suspend();
    plugin.close_editor();
    if let Some((tone, name)) = save {
        let _ = keep_tone(tone, index, "unload", name, || plugin.save_state());
    }
    // The unit holds the context too: it goes first, so the plugin's drop is what frees the context
    // (after `effClose`) and then unloads the module.
    drop(unit);
    drop(plugin);
    log::info!(
        "[plugin_host] engine slot {index} VST2 teardown: remove={remove_ms} stop+suspend+close+module={} ms",
        t.elapsed().as_millis()
    );
    Ok(())
}

#[cfg(test)]
#[path = "vst2_engine_tests.rs"]
mod tests;
