//! OWNS: a 64-bit VST2 plugin in the engine: `Vst2Unit` (the effect's process call with its rows,
//! pointer arrays, event list and time info, as an `lf_engine::SlotProcessor`) and what its owner
//! thread does with the plugin (`Vst2Plugin`, which the shared owner loop `engine_slot::serve`
//! drives): the load, resume, a restart and a re-activation after an eviction, and the teardown's
//! order, keeping the plugin's tone as `engine_slot` describes: its bank chunk
//! or its parameter values in one container (`save_state`, `restore_state`). The interface, the
//! loader and the host callback are `host/vst2.rs`.
//!
//! What differs from the other two formats, and must stay so:
//! - The plugin calls the host from any thread and leaves latches in its `HostContext`; the owner
//!   drains them every turn. Nothing the plugin reports is echoed back to it.
//! - `effStartProcess`/`effStopProcess` and `effMainsChanged` run on the OWNER; the unit's `stop`
//!   calls nothing. The only dispatcher opcode the audio thread sends is `effProcessEvents`.
//! - After `audioMasterIOChanged` the unit makes no plugin call at all until the owner has cycled
//!   the plugin (`HostContext::halted`). The note-offs the engine sends meanwhile are discarded with
//!   everything else, so the unit keeps the keys it sent the plugin and releases them itself in the
//!   first slice it processes after the cycle (`Vst2Unit::held`).
//! - A dispatcher's return is its opcode's own: 0 from `effOpen`, `effMainsChanged`,
//!   `effStartProcess`, `effStopProcess` or `effSetChunk` is no failure.
//! - After `effClose` the `AEffect` is freed memory. A unit the engine does not hand back keeps the
//!   effect, its context and its module loaded with it (`engine_slot::serve`'s teardown).

use super::engine_slot::{
    own, restore_tone, serve, Editor, EngineSlotEvent, EventSink, Loaded, OwnedPlugin, OwnerCtx, Reported, Setup,
    FAULT_EVENTS, FAULT_PARAM,
};
use super::super::editor_window::{
    create_host_window, drain_after_editor_teardown, set_client_size, show_host_window_front, HostWindow,
};
use super::super::state::ParamDesc;
use super::super::tone::{self, Vst2State, VST2_MAX_CHUNK};
use super::super::vst2::{
    open_effect, time_info, validate, EffectInfo, HostContext, OpenError, ProcessingScope, Vst2Effect, Vst2Module,
};
use super::super::vst2_abi::*;
use super::{ParamEvent, MAX_EVENTS_PER_BLOCK, MAX_PLUGIN_CHANNELS};

use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::sync::atomic::{
    AtomicU32,
    Ordering::Relaxed,
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

/// Events one slice's list holds: the notes the engine queues between two renders, and beside them
/// a note-off for every key, so the keys a halt left sounding never displace a slice's own notes.
const EVENT_ROOM: usize = MAX_SLOT_EVENTS + 128;

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
    params: Consumer<ParamEvent>,
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
    events: Box<VstEvents<EVENT_ROOM>>,
    midi: Box<[VstMidiEvent; EVENT_ROOM]>,
    /// The keys the plugin holds, a bit each: set by every note-on sent, cleared by every note-off.
    held: [u64; 2],
    /// The keys the next slice processed releases before its own events (`rearm`).
    release: [u64; 2],
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
        params: Consumer<ParamEvent>,
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
            events: Box::new(VstEvents { num_events: 0, reserved: 0, events: [std::ptr::null_mut(); EVENT_ROOM] }),
            midi: Box::new([VstMidiEvent::default(); EVENT_ROOM]),
            held: [0; 2],
            release: [0; 2],
            time: Box::new(UnsafeCell::new(time_info(0.0, 0.0))),
            position: 0,
            faults,
        });
        unit.rearm(info, max_frames);
        unit
    }

    /// Owner thread, the unit out of the engine: size the rows to what the plugin declares now (a
    /// restart can change the pin counts, a device change the block) and take the calls it was
    /// validated with. The keys the plugin still holds go to `release`: the engine's own note-offs
    /// for them may have come while the unit was halted, and it forgets its keys at every install.
    fn rearm(&mut self, info: &EffectInfo, max_frames: usize) {
        for (release, held) in self.release.iter_mut().zip(self.held.iter_mut()) {
            *release |= std::mem::take(held);
        }
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
            let ParamEvent { id, value } = event;
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

    /// Fill the event list for the slice `[at, at + len)` and return how many events it holds: first
    /// a note-off at offset 0 for every key in `release` (once: the set is emptied), then the
    /// slice's notes, note-on and note-off on channel 0, each at its offset inside the slice. The
    /// list has room for what the engine queues between two renders (`MAX_SLOT_EVENTS`) beside the
    /// releases, so a note past that is a broken caller: it is DROPPED and latched, never carried
    /// into a later slice, where it would sound late at an offset that is no longer its own.
    fn collect_notes(&mut self, events: &[SlotEvent], next: &mut usize, at: usize, len: usize) -> usize {
        let event = |delta: usize, midi_data: [u8; 4]| VstMidiEvent {
            event_type: VST_MIDI_TYPE,
            byte_size: std::mem::size_of::<VstMidiEvent>() as i32,
            delta_frames: delta as i32,
            midi_data,
            ..VstMidiEvent::default()
        };
        let mut count = 0;
        if self.release != [0; 2] {
            for key in 0..128u8 {
                if self.release[usize::from(key / 64)] & (1 << (key % 64)) != 0 {
                    self.midi[count] = event(0, [0x80, key, 0, 0]);
                    count += 1;
                }
            }
            self.release = [0; 2];
        }
        let released = count;
        while let Some(e) = events.get(*next).filter(|e| (e.offset as usize) < at + len) {
            *next += 1;
            if count - released == MAX_SLOT_EVENTS {
                self.faults.fetch_or(FAULT_EVENTS, Relaxed);
                continue;
            }
            let midi_data = match e.kind {
                SlotEventKind::NoteOn { key, velocity } => {
                    let (key, velocity) = (key & 0x7f, if velocity.is_finite() { velocity.clamp(0.0, 1.0) } else { 0.0 });
                    self.held[usize::from(key / 64)] |= 1 << (key % 64);
                    // A note-on with velocity 0 is a note-off: the quietest note is 1.
                    [0x90, key, ((velocity * 127.0).round() as u8).clamp(1, 127), 0]
                }
                SlotEventKind::NoteOff { key } => {
                    let key = key & 0x7f;
                    self.held[usize::from(key / 64)] &= !(1 << (key % 64));
                    [0x80, key, 0, 0]
                }
            };
            self.midi[count] = event((e.offset as usize).saturating_sub(at), midi_data);
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
                let list: *mut VstEvents<EVENT_ROOM> = &mut *self.events;
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

/// Everything the owner holds of a loaded plugin besides the unit. Not `Send` (`Vst2Effect`): every
/// call below runs on the owner thread. Dropping it closes the plugin in the live order: stopped
/// and suspended, the editor closed, `effClose`, then the fields in declaration order (the context
/// after `effClose`, the module LAST). The unit must be out of the engine first; when it is not,
/// the plugin is forgotten instead (`engine_slot::serve`'s teardown).
struct Vst2Plugin {
    effect: Vst2Effect,
    /// What the effect declared at its last activation (`resume`).
    info: EffectInfo,
    /// A `resume` was refused since: `info` describes an activation that is over, and no parameter
    /// is listed or read by its counts until the plugin resumes again.
    stale: bool,
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
    /// (and says so with `audioMasterIOChanged`). The ORDER after them is load-bearing: what the
    /// plugin reported from inside the two calls is consumed FIRST, and the layout is read after,
    /// so a report from another of its threads that lands after the read stays latched: the unit
    /// is then installed halted, silent, and the owner cycles it on its next turn. Cleared after
    /// the read, such a report would be lost with the rows built for the layout before it. A
    /// refusal leaves the plugin suspended and its metadata stale.
    fn resume(&mut self) -> Result<EffectInfo, String> {
        self.stale = true;
        // SAFETY: the open effect's own structure, read on its owner thread while it is suspended.
        unsafe { validate(self.effect.raw()) }?;
        // SAFETY: neither opcode reads a pointer. Their returns carry no verdict.
        unsafe {
            self.call(EFF_MAINS_CHANGED, 0, 1, 0.0);
            self.call(EFF_START_PROCESS, 0, 0, 0.0);
        }
        self.running = true;
        let _ = self.ctx.take_restart();
        self.ctx.clear_halted();
        // SAFETY: as above; nothing processes the effect yet (its unit is out of the engine).
        match unsafe { validate(self.effect.raw()) } {
            Ok(info) => {
                self.info = info;
                self.stale = false;
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

    /// `info` may not describe the plugin as it is: a resume was refused, or the plugin has reported a
    /// layout change the owner has not cycled yet. No parameter is listed or read by its counts then.
    fn unsettled(&self) -> bool {
        self.stale || self.ctx.halted()
    }

    /// The plugin's parameters: ids are indices, values normalised 0..1. VST2 names no default, so
    /// the live value stands in for it. None while the metadata is unsettled (`unsettled`).
    fn list_params(&self) -> Vec<ParamDesc> {
        let Some(get_parameter) = self.info.get_parameter.filter(|_| !self.unsettled()) else { return Vec::new() };
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
    /// of 0 or less is a plugin with nothing to save. The unit keeps processing meanwhile. While the
    /// metadata is unsettled a parameter-kind save makes no plugin call and is an error: the stored
    /// tone stays as it is, and a session export warns of the slot instead of leaving it out.
    fn save_state(&self) -> Result<Option<Vec<u8>>, String> {
        if !self.info.program_chunks() {
            if self.unsettled() {
                return Err("the plugin is between two layouts: its parameters cannot be read".to_string());
            }
            let program = self.program();
            let values = match self.info.get_parameter {
                // SAFETY: owner thread; indices the effect declared.
                Some(get_parameter) => {
                    (0..self.info.params).map(|i| unsafe { get_parameter(self.effect.raw(), i as i32) }).collect()
                }
                None => Vec::new(),
            };
            return Ok(Some(tone::encode_vst2(&Vst2State::Params { program, values })));
        }
        let program = self.program();
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
                if self.info.program_chunks() {
                    return Err("the tone holds parameters and the plugin keeps a chunk".to_string());
                }
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
                for (index, value) in values.iter().enumerate().filter(|(_, v)| v.is_finite()) {
                    // Selecting the program, or the parameter before this one, may have changed what
                    // the plugin declares: each value goes to the plugin as it is now, or not at all.
                    // SAFETY: the open effect's own structure, read on its owner thread, suspended.
                    let now = unsafe { validate(self.effect.raw()) }?;
                    if now.params != values.len() {
                        return Err(format!("the plugin went from {} parameters to {} while its tone was restored", values.len(), now.params));
                    }
                    let Some(set_parameter) = now.set_parameter else { break };
                    // SAFETY: owner thread, before the plugin processes; an index it declares now.
                    unsafe { set_parameter(self.effect.raw(), index as i32, value.clamp(0.0, 1.0)) };
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
    fn embed_editor(&mut self) -> Result<HWND, String> {
        if let Some(open) = &self.editor {
            return Ok(open.hwnd);
        }
        if !self.info.has_editor() {
            return Err("plugin has no editor".to_string());
        }
        let (width, height) = self.editor_rect().unwrap_or(PROVISIONAL_EDITOR);
        let window = create_host_window(width, height)?;
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

/// Show an editor's host window, in front. A test's window stays hidden: nothing appears or takes
/// the focus on the machine that runs it.
fn show_editor(hwnd: HWND) {
    if cfg!(not(test)) {
        show_host_window_front(hwnd);
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

/// Create the instance for `id` (the effect's unique id as 8 hex digits) on a context of its own,
/// bound to this thread as its owner. An instance this host refuses was never called, so it cannot
/// be closed: its module and its context stay loaded for the rest of the process. A shell (several
/// plugins in one file) is refused here as the scan refuses it, and closed like any open instance.
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
    let plugin = Vst2Plugin { effect, info, stale: false, running: false, editor: None, ctx, _module: module };
    let found = format!("{:08x}", info.unique_id as u32);
    if found != id {
        return Err(format!("the file holds VST2 plugin {found}, not {id}"));
    }
    // SAFETY: the opcode reads no pointer.
    if unsafe { plugin.call(EFF_GET_PLUG_CATEGORY, 0, 0, 0.0) } == PLUG_CATEGORY_SHELL {
        return Err("a VST2 shell plugin is not supported".to_string());
    }
    Ok(plugin)
}

/// Create the plugin, tell it the engine's rate and block, restore the stored tone, resume it and
/// build its unit. A tone this host cannot apply is refused before the plugin is called for it
/// (`restore_state`), so the instance that goes on is still at its defaults.
fn load(
    id: &str,
    open: impl FnOnce() -> Result<Opened, String>,
    Setup { slot, params, faults, tone, .. }: Setup<'_>,
) -> Result<Loaded<Vst2Plugin>, String> {
    let Opened { module, entry, file_stem } = open()?;
    let mut plugin = create(module, entry, id, slot)?;
    // SAFETY: the thread that opened the effect.
    let name = unsafe { plugin.effect.name() }.unwrap_or(file_stem);
    let (rate, block) = plugin.configure(slot)?;
    // `effOpen` and the two calls above may have changed what the effect declares (`create` read it
    // before any of them): the tone is checked against the counts, flags and calls it has now.
    // SAFETY: the open effect's own structure, read on its owner thread while it is suspended.
    plugin.info = unsafe { validate(plugin.effect.raw()) }?;
    // The stored tone goes in before the plugin resumes: nothing processes it yet.
    let restored = restore_tone(tone, slot.slot(), &name, |state| plugin.restore_state(state));
    let info = plugin.resume()?;
    // What the plugin reported while it opened, took its tone and resumed describes the state just
    // loaded: the parameters are listed after the load anyway.
    let _ = plugin.ctx.take_relist();
    if let Some(automation) = plugin.ctx.automation() {
        automation.drain(usize::MAX, |_, _| {});
    }
    // SAFETY: the open effect `info` was just validated from, bound to this context; the plugin
    // closes it only with the unit back in hand.
    let unit = unsafe { Vst2Unit::new(plugin.effect.raw(), plugin.ctx.clone(), &info, block, params, faults) };
    Ok(Loaded { plugin, unit, name, rate, tone: restored.report() })
}

/// The engine-mode VST2 owner (`engine_slot::serve`). `open` yields the module and its entry (a DLL
/// in production, an in-process entry in the tests); returns the teardown's result.
pub(super) fn run_with(ctx: OwnerCtx, id: &str, open: impl FnOnce() -> Result<Opened, String>) -> Result<(), String> {
    serve(ctx, |setup| load(id, open, setup))
}

impl OwnedPlugin for Vst2Plugin {
    type Unit = Vst2Unit;
    const FORMAT: &'static str = "VST2";

    /// `audioMasterIOChanged` only raised latches (on any thread, the audio thread included) and
    /// silenced the unit; the owner cycles the plugin before anything reads it by its counts.
    fn take_restart(&mut self) -> Option<String> {
        self.ctx.take_restart().then(|| "audioMasterIOChanged".to_string())
    }

    /// Re-activate the plugin at the engine's current terms and install its unit, rows rebuilt. On
    /// failure the unit comes back to the caller and the plugin is left suspended.
    fn reinstall(&mut self, mut unit: Box<Vst2Unit>, slot: &SlotHost) -> Result<(), (Box<Vst2Unit>, String)> {
        self.suspend();
        let activated = self.configure(slot).and_then(|terms| Ok((self.resume()?, terms)));
        match activated {
            Ok((info, (rate, block))) => {
                unit.rearm(&info, block);
                slot.install(unit, rate).map_err(|(unit, e)| {
                    // Resumed for an engine that did not take the unit: suspended, as every failed cycle ends.
                    self.suspend();
                    (own(unit), e)
                })
            }
            Err(e) => Err((unit, e)),
        }
    }

    /// What the plugin's own editor moved, with the value the plugin reported: told to the caller,
    /// kept for the tone, never read back from the plugin or sent to it again. Nothing is told by a
    /// count the plugin no longer answers to.
    fn turn(&mut self, sink: &EventSink) -> Reported {
        let (mut changed, declared) = (false, if self.unsettled() { 0 } else { self.info.params });
        if let Some(automation) = self.ctx.automation() {
            automation.drain(declared, |param, value| {
                changed = true;
                sink(EngineSlotEvent::ParamChanged { id: param as u32, value: f64::from(value) });
            });
        }
        Reported { relist: self.ctx.take_relist(), changed }
    }

    fn idle(&mut self) -> bool {
        if self.ctx.take_need_idle() {
            // SAFETY: the opcode reads no pointer.
            unsafe { self.call(EFF_IDLE, 0, 0, 0.0) };
        }
        // A size the plugin asked for from a thread that may not touch the window. With no editor
        // open there is nothing to size, and the request is dropped.
        let resize = self.ctx.take_resize();
        let Some(window) = &self.editor else { return false };
        // SAFETY: the opcode reads no pointer.
        unsafe { self.call(EFF_EDIT_IDLE, 0, 0, 0.0) };
        if let Some((width, height)) = resize {
            let _ = set_client_size(window.hwnd, width as u32, height as u32);
        }
        let closed = window.close_requested();
        if closed {
            self.close_editor();
        }
        closed
    }

    fn params(&mut self) -> Result<Vec<ParamDesc>, String> {
        Ok(self.list_params())
    }

    fn state(&mut self) -> Result<Option<Vec<u8>>, String> {
        self.save_state()
    }

    fn editor(&self) -> Editor {
        if self.editor.is_some() { Editor::Hosted } else { Editor::Closed }
    }

    fn editor_open(&mut self) -> Result<(), String> {
        self.embed_editor().map(show_editor)
    }

    fn editor_close(&mut self) {
        self.close_editor();
    }

    fn quiesce(&mut self) {
        self.suspend();
    }

    /// The unit holds the context too: it goes first, so the plugin's drop (stopped, suspended,
    /// `effClose`) is what frees the context and then unloads the module.
    fn release(self, unit: Option<Box<Vst2Unit>>) {
        drop(unit);
        drop(self);
    }
}

#[cfg(test)]
#[path = "vst2_engine_tests.rs"]
mod tests;
