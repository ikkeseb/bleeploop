//! A VST2 effect implemented in Rust behind the real `AEffect` layout and a real entry function, so
//! `open_effect`, the host callback, the engine unit and its owner are exercised through the binary
//! interface with no DLL. The entry builds whatever `Shape` the test set for its thread (a valid
//! stereo effect by default, or one with a field this host must refuse), calls the host back from
//! inside the "constructor" the ways real plugins do, and records what it was told and every
//! dispatcher call it got. With a `Probe` in its shape the instance also plays, keeps parameters,
//! programs, a chunk and an editor rectangle, calls the host back from where real plugins do, and
//! records what the host did to it (the engine-mode tests, `vst2_engine_tests.rs`).
use super::super::vst2_abi::*;
use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::sync::atomic::{
    AtomicBool, AtomicI32, AtomicIsize, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize,
    Ordering::{Relaxed, SeqCst},
};
use std::sync::Mutex;
use std::thread::ThreadId;

/// The thread every engine-mode test renders on (`engine_io::test_rig`).
const TEST_DEVICE_THREAD: &str = "lf-test-device";

/// What the next entry call on this thread builds.
#[derive(Clone, Copy)]
pub(crate) struct Shape {
    /// The entry returns no effect at all.
    pub(crate) null: bool,
    pub(crate) magic: i32,
    pub(crate) flags: i32,
    pub(crate) inputs: i32,
    pub(crate) outputs: i32,
    pub(crate) params: i32,
    pub(crate) programs: i32,
    pub(crate) delay: i32,
    pub(crate) unique_id: i32,
    /// Whether each function pointer is there.
    pub(crate) dispatcher: bool,
    pub(crate) process: bool,
    pub(crate) process_replacing: bool,
    pub(crate) set_parameter: bool,
    pub(crate) get_parameter: bool,
    /// What `effGetEffectName` and `effGetProductString` write (empty: nothing).
    pub(crate) effect_name: &'static str,
    pub(crate) product: &'static str,
    /// `effGetPlugCategory`'s answer.
    pub(crate) category: isize,
    /// Runs inside the entry, between its first callback and the rest.
    pub(crate) during_entry: Option<fn()>,
    /// What the instance does and records once it runs; `None`: it renders nothing.
    pub(crate) probe: Option<&'static Probe>,
}

impl Default for Shape {
    fn default() -> Self {
        Shape {
            null: false,
            magic: EFFECT_MAGIC,
            flags: EFF_FLAGS_CAN_REPLACING,
            inputs: 2,
            outputs: 2,
            params: 3,
            programs: 1,
            delay: 0,
            unique_id: 0x4c66_4678,
            dispatcher: true,
            process: true,
            process_replacing: true,
            set_parameter: true,
            get_parameter: true,
            effect_name: "Fixture",
            product: "Fixture Product",
            category: 1,
            during_entry: None,
            probe: None,
        }
    }
}

/// Parameters a probed instance keeps a value for.
pub(crate) const PROBE_PARAMS: usize = 8;

/// Events a probed instance keeps in its trace; later ones are counted and not kept.
const EVENT_TRACE: usize = 1024;

/// One event a probed instance got: the process call it came before (how many this instance had
/// rendered by then), its offset in that slice, and its MIDI status and key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TracedEvent {
    pub(crate) slice: usize,
    pub(crate) delta: i32,
    pub(crate) status: u8,
    pub(crate) key: u8,
}

/// One dispatcher call a probed instance got: the opcode, its `value`, the thread it came on, and
/// whether that thread was the test device's (the audio thread).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Call {
    pub(crate) opcode: i32,
    pub(crate) value: isize,
    pub(crate) thread: ThreadId,
    pub(crate) on_device: bool,
}

/// What a test sets a probed instance to do, and what the instance saw. A test leaks one per plugin
/// (`Probe::new`), since the instance, its owner thread and the test all read it. Everything a
/// process, parameter or event call touches is an atomic: those run on the audio thread, under the
/// allocation counter in one test.
#[derive(Default)]
pub(crate) struct Probe {
    // ── What the plugin does ─────────────────────────────────────────────────────────────────
    /// An instrument's output level (`f32` bits), written to its first `signal_outputs` outputs;
    /// an instance with inputs echoes them instead.
    pub(crate) level: AtomicU32,
    /// How many outputs carry the level (0: all of them); the rest stay silent.
    pub(crate) signal_outputs: AtomicI32,
    /// How far the first pair stands apart (`f32` bits): the first output plays the level plus
    /// this, the second the level minus it.
    pub(crate) spread: AtomicU32,
    /// `setParameter` reports half the value it was given through `audioMasterAutomate`.
    pub(crate) automate_in_set: AtomicBool,
    /// The next process call changes the output count to this and calls `audioMasterIOChanged`.
    pub(crate) grow_in_process: AtomicI32,
    /// The next process call first raises its input and output counts to these (`inputs << 8 |
    /// outputs`, 0: no growth), reads a sample from every input row it just declared and fills
    /// every output row it just declared, renders, then calls `audioMasterIOChanged`.
    pub(crate) grow_before_render: AtomicI32,
    /// `effOpen` changes the parameter count to this (`-1`: it stays).
    pub(crate) params_on_open: AtomicI32,
    /// `effMainsChanged(0)` changes the output count, and the parameter count, to this (`-1`: it
    /// stays).
    pub(crate) outputs_on_suspend: AtomicI32,
    pub(crate) params_on_suspend: AtomicI32,
    /// `effSetProgram` changes the parameter count to this (`-1`: it stays).
    pub(crate) params_on_set_program: AtomicI32,
    /// `effMainsChanged(1)` calls `audioMasterIOChanged`, as a plugin that settles its pins there.
    pub(crate) io_changed_in_resume: AtomicBool,
    /// The next `effMainsChanged(1)` changes the layout to this (inputs, outputs, latency) and
    /// calls `audioMasterIOChanged`.
    pub(crate) layout_on_resume: Mutex<Option<(i32, i32, i32)>>,
    /// `effGetChunk` answers its length with no pointer.
    pub(crate) chunk_null: AtomicBool,
    /// What `effGetChunk` answers as its length (0: the chunk's own).
    pub(crate) chunk_len: AtomicIsize,
    /// `effSetProgram` resets every parameter to 0 and clears the chunk, as selecting a program
    /// replaces a plugin's whole state.
    pub(crate) program_resets: AtomicBool,
    /// The editor's size before `effEditOpen` and once it is open (width, height); `no_rect`:
    /// `effEditGetRect` answers no rectangle at all.
    pub(crate) rect: Mutex<(i16, i16)>,
    pub(crate) rect_open: Mutex<(i16, i16)>,
    pub(crate) no_rect: AtomicBool,
    pub(crate) refuse_open: AtomicBool,
    /// `effEditOpen` asks the host for this size (width, height) through `audioMasterSizeWindow`.
    pub(crate) size_in_open: Mutex<Option<(i32, isize)>>,
    /// Runs once inside the next `effIdle`, on the thread that called it, with the effect.
    #[allow(clippy::type_complexity)]
    pub(crate) on_idle: Mutex<Option<Box<dyn FnOnce(*mut AEffect) + Send>>>,

    // ── The plugin's state ───────────────────────────────────────────────────────────────────
    pub(crate) values: [AtomicU32; PROBE_PARAMS],
    pub(crate) program: AtomicI32,
    pub(crate) chunk: Mutex<Vec<u8>>,
    /// The bank `effGetChunk` last answered: the chunk, then one byte for the current program.
    /// `effSetChunk` takes a bank apart the same way, so a bank restores its own program.
    bank: Mutex<Vec<u8>>,
    editor_open: AtomicBool,
    /// Between `effStartProcess` and `effStopProcess`, and how many process calls are in flight.
    started: AtomicBool,
    in_process: AtomicUsize,
    /// An `effProcessEvents` no process call has followed yet.
    events_pending: AtomicBool,
    /// Between an `audioMasterIOChanged` this instance raised and the `effMainsChanged(0)` that
    /// answers it.
    awaiting_cycle: AtomicBool,

    // ── What the host did ────────────────────────────────────────────────────────────────────
    /// The live instance (null once closed) and how many the entry made and `effClose` freed.
    pub(crate) effect: AtomicPtr<AEffect>,
    pub(crate) instances: AtomicUsize,
    pub(crate) closes: AtomicUsize,
    /// Every dispatcher call but `effProcessEvents` and `effEditIdle`, in order.
    pub(crate) calls: Mutex<Vec<Call>>,
    pub(crate) processes: AtomicUsize,
    pub(crate) last_frames: AtomicI32,
    /// Process, event and parameter calls that came while this instance waited for its restart.
    pub(crate) calls_while_halted: AtomicUsize,
    /// Process and event calls on a thread other than the test device's.
    pub(crate) rt_calls_off_device: AtomicUsize,
    /// Process calls that came while the plugin was not started, and lifecycle calls (stop, mains,
    /// close, sample rate, block size, set chunk) that came while a process call was in flight.
    pub(crate) renders_while_stopped: AtomicUsize,
    pub(crate) lifecycle_in_process: AtomicUsize,
    /// What `grow_before_render` found: the input rows it read, how many of them held anything
    /// but silence, and the output rows it filled.
    pub(crate) grown_inputs_read: AtomicUsize,
    pub(crate) grown_inputs_loud: AtomicUsize,
    pub(crate) grown_outputs_filled: AtomicUsize,
    /// Distinct row pointers among the declared inputs and outputs of the last process call, and
    /// whether all 64 entries of both arrays were non-null.
    pub(crate) distinct_inputs: AtomicUsize,
    pub(crate) distinct_outputs: AtomicUsize,
    pub(crate) rows_missing: AtomicBool,
    /// What `audioMasterGetTime` and `audioMasterGetCurrentProcessLevel` answered inside the last
    /// process call (`f64` bits for the position and the rate).
    pub(crate) time_pos: AtomicU64,
    pub(crate) time_rate: AtomicU64,
    pub(crate) time_tempo: AtomicU64,
    pub(crate) time_flags: AtomicI32,
    pub(crate) process_level: AtomicIsize,
    pub(crate) event_calls: AtomicUsize,
    /// Process calls an `effProcessEvents` came before, and event calls that followed another with
    /// no process call between them.
    pub(crate) slices_after_events: AtomicUsize,
    pub(crate) event_calls_unanswered: AtomicUsize,
    /// Every event, in order (`TracedEvent`, packed), and how many came.
    event_trace: Box<[AtomicU64]>,
    events_traced: AtomicUsize,
    pub(crate) note_ons: AtomicUsize,
    pub(crate) note_offs: AtomicUsize,
    pub(crate) last_key: AtomicI32,
    pub(crate) last_velocity: AtomicI32,
    pub(crate) last_delta: AtomicI32,
    /// Events that were not a well-formed MIDI event on channel 0.
    pub(crate) bad_events: AtomicUsize,
    pub(crate) sets: AtomicUsize,
    pub(crate) sets_on_device: AtomicUsize,
    pub(crate) gets: AtomicUsize,
    pub(crate) last_set_index: AtomicI32,
    pub(crate) last_set_value: AtomicU32,
    /// The last `effSetSampleRate` (`f32` bits) and `effSetBlockSize`, and what the host answered
    /// `audioMasterGetSampleRate` inside the last `effMainsChanged(1)`.
    pub(crate) rate: AtomicU32,
    pub(crate) block: AtomicIsize,
    pub(crate) host_rate_at_resume: AtomicIsize,
    pub(crate) set_programs: AtomicUsize,
    pub(crate) set_chunks: AtomicUsize,
    /// The `index` the last `effGetChunk` and `effSetChunk` came with (`-1`: none yet; 0 is the bank).
    pub(crate) get_chunk_index: AtomicI32,
    pub(crate) set_chunk_index: AtomicI32,
    pub(crate) edit_opens: AtomicUsize,
    pub(crate) edit_closes: AtomicUsize,
    pub(crate) edit_idles: AtomicUsize,
    pub(crate) idles: AtomicUsize,
    /// The window `effEditOpen` was given, and what the host answered each size request it made.
    pub(crate) editor_parent: AtomicIsize,
    pub(crate) size_answers: Mutex<Vec<isize>>,
}

impl Probe {
    /// A probe for one plugin, alive for the rest of the test process.
    pub(crate) fn new() -> &'static Probe {
        let mut probe = Probe::default();
        probe.event_trace = (0..EVENT_TRACE).map(|_| AtomicU64::new(0)).collect();
        for unset in [&probe.outputs_on_suspend, &probe.params_on_suspend, &probe.params_on_set_program, &probe.params_on_open, &probe.get_chunk_index, &probe.set_chunk_index] {
            unset.store(-1, Relaxed);
        }
        *probe.rect.lock().unwrap() = (400, 300);
        *probe.rect_open.lock().unwrap() = (400, 300);
        Box::leak(Box::new(probe))
    }

    pub(crate) fn set_level(&self, level: f32) {
        self.level.store(level.to_bits(), Relaxed);
    }

    pub(crate) fn value(&self, index: usize) -> f32 {
        f32::from_bits(self.values[index].load(Relaxed))
    }

    /// The dispatcher calls so far, as `(opcode, value)`.
    pub(crate) fn opcodes(&self) -> Vec<(i32, isize)> {
        self.calls.lock().unwrap().iter().map(|call| (call.opcode, call.value)).collect()
    }

    /// How many calls of `opcode` with `value` came so far.
    pub(crate) fn count(&self, opcode: i32, value: isize) -> usize {
        self.calls.lock().unwrap().iter().filter(|call| call.opcode == opcode && call.value == value).count()
    }

    /// The live instance's effect, for a test that calls the host as the plugin would.
    pub(crate) fn raw(&self) -> *mut AEffect {
        self.effect.load(SeqCst)
    }

    /// Change the live instance's output count from outside.
    ///
    /// # Safety
    /// Nothing reads the effect meanwhile: its unit is out of the engine and its owner is idle.
    pub(crate) unsafe fn set_outputs(&self, outputs: i32) {
        // SAFETY: the caller's contract; the instance is alive (`raw` is not null).
        unsafe { (*self.raw()).num_outputs = outputs };
    }

    /// Every event the instance got, in order (the first `EVENT_TRACE` of them).
    pub(crate) fn events(&self) -> Vec<TracedEvent> {
        let kept = self.events_traced.load(Relaxed).min(EVENT_TRACE);
        self.event_trace[..kept]
            .iter()
            .map(|packed| {
                let packed = packed.load(Relaxed);
                TracedEvent { slice: (packed >> 32) as usize, delta: i32::from((packed >> 16) as u16 as i16), status: (packed >> 8) as u8, key: packed as u8 }
            })
            .collect()
    }

    /// The `skip + 1`-th `validate` of the live instance from now is followed, before the host does
    /// anything else, by what a thread of the plugin's own could do right then: the output count
    /// becomes `outputs` and `audioMasterIOChanged` is called (`validated`).
    pub(crate) fn io_change_after_validate(&'static self, skip: usize, outputs: i32) {
        *AFTER_VALIDATE.lock().unwrap() = Some(AfterValidate { effect: self.raw() as usize, skip, outputs, probe: self });
    }

    fn on_device() -> bool {
        std::thread::current().name() == Some(TEST_DEVICE_THREAD)
    }

    /// A process, event or parameter call arrived.
    fn rt_call(&self) {
        if self.awaiting_cycle.load(SeqCst) {
            self.calls_while_halted.fetch_add(1, Relaxed);
        }
    }
}

/// What `Probe::io_change_after_validate` armed.
struct AfterValidate {
    effect: usize,
    skip: usize,
    outputs: i32,
    probe: &'static Probe,
}

static AFTER_VALIDATE: Mutex<Option<AfterValidate>> = Mutex::new(None);

/// Called by `vst2::validate` once it has read `effect` (test builds only): the one point between
/// two host steps that no plugin call reaches, where a plugin thread can still move.
pub(crate) fn validated(effect: *const AEffect) {
    let mut armed = AFTER_VALIDATE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(hook) = armed.as_mut().filter(|hook| hook.effect == effect as usize) else { return };
    if hook.skip > 0 {
        hook.skip -= 1;
        return;
    }
    let Some(hook) = armed.take() else { return };
    drop(armed);
    let effect = effect.cast_mut();
    // SAFETY: the armed effect is this fixture's live instance, which changes its own layout and
    // tells its host, as from a thread of its own; the opcode passes no pointer.
    unsafe {
        (*effect).num_outputs = hook.outputs;
        hook.probe.awaiting_cycle.store(true, SeqCst);
        call_host(effect, AUDIO_MASTER_IO_CHANGED, 0, 0, 0.0);
    }
}

/// Counts a process call in flight for as long as it lives.
struct InFlight<'a>(&'a AtomicUsize);

impl<'a> InFlight<'a> {
    fn enter(count: &'a AtomicUsize) -> Self {
        count.fetch_add(1, SeqCst);
        Self(count)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, SeqCst);
    }
}

/// One instance: the `AEffect` first, so the effect's pointer is the instance's.
#[repr(C)]
struct Instance {
    effect: AEffect,
    shape: Shape,
    host: HostCallback,
    /// The rectangle `effEditGetRect` points the host at: the plugin's own memory.
    rect: ERect,
}

thread_local! {
    static NEXT: Cell<Option<Shape>> = const { Cell::new(None) };
    /// What the host answered the entry's callbacks, in order.
    static SEEN: RefCell<Vec<isize>> = const { RefCell::new(Vec::new()) };
    /// Every dispatcher call on this thread to an instance without a probe: the opcode, and
    /// whether `resvd1` was set by then.
    static CALLS: RefCell<Vec<(i32, bool)>> = const { RefCell::new(Vec::new()) };
}

/// `setParameter` and `getParameter` calls on any fixture instance without a probe, from any thread.
static PARAMETER_CALLS: AtomicUsize = AtomicUsize::new(0);

/// Run `f` with `shape` as what the entry builds on this thread; the outer shape comes back after.
pub(crate) fn with_shape<R>(shape: Shape, f: impl FnOnce() -> R) -> R {
    let outer = NEXT.replace(Some(shape));
    let result = f();
    NEXT.set(outer);
    result
}

/// `shape` is what the entry builds on this thread from now on (an owner thread a test spawned).
pub(crate) fn arm(shape: Shape) {
    NEXT.set(Some(shape));
}

/// What the host answered the entries run on this thread since the last take: per entry, the sample
/// rate asked with no effect, (then `during_entry`), the rate asked with the still unbound effect,
/// and the block size asked both ways.
pub(crate) fn take_seen() -> Vec<isize> {
    SEEN.take()
}

/// The dispatcher calls this thread's instances got since the last take.
pub(crate) fn take_calls() -> Vec<(i32, bool)> {
    CALLS.take()
}

pub(crate) fn parameter_calls() -> usize {
    PARAMETER_CALLS.load(Relaxed)
}

/// The fixture's `VSTPluginMain`.
pub(crate) unsafe extern "C" fn entry(host: HostCallback) -> *mut AEffect {
    let shape = NEXT.get().unwrap_or_default();
    let ask = |effect: *mut AEffect, opcode: i32| {
        // SAFETY: the host's callback, with an opcode that reads no pointer.
        let answer = unsafe { host(effect, opcode, 0, 0, std::ptr::null_mut(), 0.0) };
        SEEN.with_borrow_mut(|seen| seen.push(answer));
    };
    // A constructor asking before it has an effect to name.
    ask(std::ptr::null_mut(), AUDIO_MASTER_GET_SAMPLE_RATE);
    if let Some(during) = shape.during_entry {
        during();
    }
    let instance = Box::into_raw(Box::new(Instance {
        effect: AEffect {
            magic: shape.magic,
            dispatcher: shape.dispatcher.then_some(dispatcher as DispatcherFn),
            process: shape.process.then_some(process_accumulating as ProcessFn),
            set_parameter: shape.set_parameter.then_some(set_parameter as SetParameterFn),
            get_parameter: shape.get_parameter.then_some(get_parameter as GetParameterFn),
            num_programs: shape.programs,
            num_params: shape.params,
            num_inputs: shape.inputs,
            num_outputs: shape.outputs,
            flags: shape.flags,
            resvd1: 0,
            resvd2: 0,
            initial_delay: shape.delay,
            real_qualities: 0,
            off_qualities: 0,
            io_ratio: 1.0,
            object: std::ptr::null_mut(),
            user: std::ptr::null_mut(),
            unique_id: shape.unique_id,
            version: 1,
            process_replacing: shape.process_replacing.then_some(process_replacing as ProcessFn),
            process_double_replacing: None,
            future: [0; 56],
        },
        shape,
        host,
        rect: ERect::default(),
    }));
    let effect = instance.cast::<AEffect>();
    // SAFETY: the instance just boxed; `object` is the plugin's own pointer to itself.
    unsafe { (*effect).object = instance.cast() };
    // A constructor asking with its effect, which the host has not bound to a context yet.
    ask(effect, AUDIO_MASTER_GET_SAMPLE_RATE);
    ask(std::ptr::null_mut(), AUDIO_MASTER_GET_BLOCK_SIZE);
    ask(effect, AUDIO_MASTER_GET_BLOCK_SIZE);
    if shape.null {
        // SAFETY: the instance boxed above, handed to nobody.
        drop(unsafe { Box::from_raw(instance) });
        return std::ptr::null_mut();
    }
    if let Some(probe) = shape.probe {
        probe.instances.fetch_add(1, Relaxed);
        probe.effect.store(effect, SeqCst);
    }
    effect
}

/// The instance's host callback, with an opcode that passes no pointer.
///
/// # Safety
/// `effect` is a live fixture instance.
unsafe fn call_host(effect: *mut AEffect, opcode: i32, index: i32, value: isize, opt: f32) -> isize {
    // SAFETY: the caller's contract; the instance keeps the callback its entry was given.
    unsafe { ((*effect.cast::<Instance>()).host)(effect, opcode, index, value, std::ptr::null_mut(), opt) }
}

unsafe extern "C" fn dispatcher(effect: *mut AEffect, opcode: i32, index: i32, value: isize, ptr: *mut c_void, opt: f32) -> isize {
    // SAFETY: the host passes the effect this fixture made, whose pointer is its instance's.
    let (bound, shape) = unsafe { ((*effect).resvd1 != 0, (*effect.cast::<Instance>()).shape) };
    let write = |text: &str| {
        if !text.is_empty() {
            // SAFETY: the host's string buffer, far larger than any fixture name plus its NUL.
            unsafe {
                std::ptr::copy_nonoverlapping(text.as_ptr(), ptr.cast::<u8>(), text.len());
                *ptr.cast::<u8>().add(text.len()) = 0;
            }
        }
        isize::from(!text.is_empty())
    };
    if let Some(probe) = shape.probe {
        // SAFETY: the host's call on this fixture's live instance, `ptr` being what `opcode` defines.
        if let Some(answer) = unsafe { probed(probe, effect, opcode, index, value, ptr, opt) } {
            return answer;
        }
    } else {
        CALLS.with_borrow_mut(|calls| calls.push((opcode, bound)));
    }
    match opcode {
        EFF_CLOSE => {
            // SAFETY: `effClose` frees the instance; the host never calls it again.
            drop(unsafe { Box::from_raw(effect.cast::<Instance>()) });
            0
        }
        EFF_GET_EFFECT_NAME => write(shape.effect_name),
        EFF_GET_PRODUCT_STRING => write(shape.product),
        EFF_GET_PLUG_CATEGORY => shape.category,
        // `effOpen` answers 0, as real plugins do: it is not a failure. So do `effMainsChanged`,
        // `effStartProcess`, `effStopProcess` and `effSetChunk`.
        _ => 0,
    }
}

/// A probed instance's dispatcher: `Some` when the call is answered here, `None` for the opcodes
/// every instance shares.
///
/// # Safety
/// As the dispatcher: `effect` is this fixture's live instance and `ptr` is what `opcode` defines.
unsafe fn probed(probe: &Probe, effect: *mut AEffect, opcode: i32, index: i32, value: isize, ptr: *mut c_void, opt: f32) -> Option<isize> {
    // The two opcodes that come every block or every turn record through atomics alone.
    if opcode == EFF_PROCESS_EVENTS {
        probe.rt_call();
        if !Probe::on_device() {
            probe.rt_calls_off_device.fetch_add(1, Relaxed);
        }
        probe.event_calls.fetch_add(1, Relaxed);
        if probe.events_pending.swap(true, Relaxed) {
            probe.event_calls_unanswered.fetch_add(1, Relaxed);
        }
        let slice = probe.processes.load(Relaxed) as u64;
        // SAFETY: the host's event list: `num_events` pointers to MIDI events after its header.
        unsafe {
            let list = ptr.cast::<VstEvents>();
            let entries = (&raw const (*list).events).cast::<*mut VstMidiEvent>();
            for i in 0..(*list).num_events as usize {
                let event = **entries.add(i);
                let [status, key, velocity, _] = event.midi_data;
                let sound = event.event_type == VST_MIDI_TYPE && event.byte_size == 32 && key < 128;
                match status {
                    0x90 if sound && (1..=127).contains(&velocity) => probe.note_ons.fetch_add(1, Relaxed),
                    0x80 if sound => probe.note_offs.fetch_add(1, Relaxed),
                    _ => probe.bad_events.fetch_add(1, Relaxed),
                };
                probe.last_key.store(i32::from(key), Relaxed);
                probe.last_velocity.store(i32::from(velocity), Relaxed);
                probe.last_delta.store(event.delta_frames, Relaxed);
                if let Some(entry) = probe.event_trace.get(probe.events_traced.fetch_add(1, Relaxed)) {
                    let delta = u64::from(event.delta_frames as i16 as u16);
                    entry.store(slice << 32 | delta << 16 | u64::from(status) << 8 | u64::from(key), Relaxed);
                }
            }
        }
        return Some(1);
    }
    if opcode == EFF_EDIT_IDLE {
        probe.edit_idles.fetch_add(1, Relaxed);
        return Some(0);
    }
    probe.calls.lock().unwrap().push(Call { opcode, value, thread: std::thread::current().id(), on_device: Probe::on_device() });
    let lifecycle = matches!(opcode, EFF_STOP_PROCESS | EFF_MAINS_CHANGED | EFF_CLOSE | EFF_SET_SAMPLE_RATE | EFF_SET_BLOCK_SIZE | EFF_SET_CHUNK);
    if lifecycle && probe.in_process.load(SeqCst) != 0 {
        probe.lifecycle_in_process.fetch_add(1, Relaxed);
    }
    // SAFETY: the instance is alive for every opcode handled below (`effClose` is not).
    let instance = unsafe { &mut *effect.cast::<Instance>() };
    match opcode {
        EFF_OPEN => {
            let params = probe.params_on_open.load(Relaxed);
            if params >= 0 {
                instance.effect.num_params = params;
            }
            None
        }
        EFF_START_PROCESS => {
            probe.started.store(true, SeqCst);
            Some(0)
        }
        EFF_STOP_PROCESS => {
            probe.started.store(false, SeqCst);
            Some(0)
        }
        EFF_CLOSE => {
            probe.closes.fetch_add(1, Relaxed);
            probe.effect.store(std::ptr::null_mut(), SeqCst);
            None
        }
        EFF_SET_PROGRAM => {
            probe.set_programs.fetch_add(1, Relaxed);
            probe.program.store(value as i32, Relaxed);
            let params = probe.params_on_set_program.swap(-1, Relaxed);
            if params >= 0 {
                instance.effect.num_params = params;
            }
            if probe.program_resets.load(Relaxed) {
                for stored in &probe.values {
                    stored.store(0f32.to_bits(), Relaxed);
                }
                probe.chunk.lock().unwrap().clear();
            }
            Some(0)
        }
        EFF_GET_PROGRAM => Some(probe.program.load(Relaxed) as isize),
        EFF_GET_PARAM_NAME => {
            // Longer than the 8 bytes the format promises, as real plugins write.
            let name = format!("Fixture parameter {index}\0");
            // SAFETY: the host's string buffer, far larger than this name.
            unsafe { std::ptr::copy_nonoverlapping(name.as_ptr(), ptr.cast::<u8>(), name.len()) };
            Some(0)
        }
        EFF_SET_SAMPLE_RATE => {
            probe.rate.store(opt.to_bits(), Relaxed);
            Some(0)
        }
        EFF_SET_BLOCK_SIZE => {
            probe.block.store(value, Relaxed);
            Some(0)
        }
        EFF_MAINS_CHANGED if value == 0 => {
            let outputs = probe.outputs_on_suspend.swap(-1, Relaxed);
            if outputs >= 0 {
                instance.effect.num_outputs = outputs;
            }
            let params = probe.params_on_suspend.swap(-1, Relaxed);
            if params >= 0 {
                instance.effect.num_params = params;
            }
            probe.awaiting_cycle.store(false, SeqCst);
            Some(0)
        }
        EFF_MAINS_CHANGED => {
            let layout = probe.layout_on_resume.lock().unwrap().take();
            if let Some((inputs, outputs, delay)) = layout {
                instance.effect.num_inputs = inputs;
                instance.effect.num_outputs = outputs;
                instance.effect.initial_delay = delay;
            }
            // SAFETY: this live instance; the opcodes pass no pointer.
            unsafe {
                probe.host_rate_at_resume.store(call_host(effect, AUDIO_MASTER_GET_SAMPLE_RATE, 0, 0, 0.0), Relaxed);
                if layout.is_some() || probe.io_changed_in_resume.load(Relaxed) {
                    call_host(effect, AUDIO_MASTER_IO_CHANGED, 0, 0, 0.0);
                }
            }
            Some(0)
        }
        EFF_EDIT_GET_RECT => {
            if probe.no_rect.load(Relaxed) {
                return Some(0);
            }
            let reported = if probe.editor_open.load(Relaxed) { &probe.rect_open } else { &probe.rect };
            let (width, height) = *reported.lock().unwrap();
            instance.rect = ERect { top: 0, left: 0, bottom: height, right: width };
            // SAFETY: the host passes where to write the pointer to the plugin's own rectangle.
            unsafe { *ptr.cast::<*mut ERect>() = &raw mut instance.rect };
            Some(1)
        }
        EFF_EDIT_OPEN => {
            if probe.refuse_open.load(Relaxed) {
                return Some(0);
            }
            probe.edit_opens.fetch_add(1, Relaxed);
            probe.editor_parent.store(ptr as isize, Relaxed);
            probe.editor_open.store(true, Relaxed);
            if let Some((width, height)) = *probe.size_in_open.lock().unwrap() {
                // SAFETY: this live instance; the opcode passes no pointer.
                let answer = unsafe { call_host(effect, AUDIO_MASTER_SIZE_WINDOW, width, height, 0.0) };
                probe.size_answers.lock().unwrap().push(answer);
            }
            Some(1)
        }
        EFF_EDIT_CLOSE => {
            probe.edit_closes.fetch_add(1, Relaxed);
            probe.editor_open.store(false, Relaxed);
            Some(0)
        }
        EFF_GET_CHUNK => {
            probe.get_chunk_index.store(index, Relaxed);
            let chunk = probe.chunk.lock().unwrap();
            let mut bank = probe.bank.lock().unwrap();
            bank.clear();
            if !chunk.is_empty() {
                bank.extend_from_slice(&chunk);
                bank.push(probe.program.load(Relaxed) as u8);
            }
            let data = if probe.chunk_null.load(Relaxed) { std::ptr::null() } else { bank.as_ptr() };
            // SAFETY: the host passes where to write the pointer to the plugin's own bank, which
            // stays where it is until the next `effGetChunk`.
            unsafe { *ptr.cast::<*const u8>() = data };
            Some(match probe.chunk_len.load(Relaxed) {
                0 => bank.len() as isize,
                lie => lie,
            })
        }
        EFF_SET_CHUNK => {
            probe.set_chunks.fetch_add(1, Relaxed);
            probe.set_chunk_index.store(index, Relaxed);
            // SAFETY: the host passes `value` bytes at `ptr`.
            let bank = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), value as usize) };
            if let Some((&program, chunk)) = bank.split_last() {
                probe.program.store(i32::from(program), Relaxed);
                *probe.chunk.lock().unwrap() = chunk.to_vec();
            }
            Some(0)
        }
        EFF_IDLE => {
            probe.idles.fetch_add(1, Relaxed);
            let queued = probe.on_idle.lock().unwrap().take();
            if let Some(run) = queued {
                run(effect);
            }
            Some(0)
        }
        _ => None,
    }
}

unsafe extern "C" fn process_replacing(effect: *mut AEffect, inputs: *mut *mut f32, outputs: *mut *mut f32, frames: i32) {
    // SAFETY: the host's process call on this fixture's instance.
    unsafe { render(effect, inputs, outputs, frames, false) }
}

unsafe extern "C" fn process_accumulating(effect: *mut AEffect, inputs: *mut *mut f32, outputs: *mut *mut f32, frames: i32) {
    // SAFETY: the host's process call on this fixture's instance.
    unsafe { render(effect, inputs, outputs, frames, true) }
}

/// A probed instance's process call: an instance with inputs echoes each to the output of the same
/// index, one without writes its level; `add` adds into the outputs (the accumulating call), else
/// every declared output is overwritten. Atomics only, no allocation.
///
/// # Safety
/// As a process call: both arrays hold the host's rows of at least `frames` frames.
unsafe fn render(effect: *mut AEffect, inputs: *mut *mut f32, outputs: *mut *mut f32, frames: i32, add: bool) {
    // SAFETY: the host passes the effect this fixture made.
    let Some(probe) = (unsafe { (*effect.cast::<Instance>()).shape.probe }) else { return };
    probe.rt_call();
    if !Probe::on_device() {
        probe.rt_calls_off_device.fetch_add(1, Relaxed);
    }
    if !probe.started.load(SeqCst) {
        probe.renders_while_stopped.fetch_add(1, Relaxed);
    }
    let _in_flight = InFlight::enter(&probe.in_process);
    if probe.events_pending.swap(false, Relaxed) {
        probe.slices_after_events.fetch_add(1, Relaxed);
    }
    // SAFETY (each call): the live instance's own counts.
    let declared = |effect: *mut AEffect| unsafe { ((*effect).num_inputs.max(0) as usize, (*effect).num_outputs.max(0) as usize) };
    let before = declared(effect);
    let grown = probe.grow_before_render.swap(0, Relaxed);
    if grown != 0 {
        // SAFETY: this live instance changes its own layout before it renders, as a plugin can.
        unsafe {
            (*effect).num_inputs = grown >> 8;
            (*effect).num_outputs = grown & 0xff;
        }
    }
    let (ins, outs) = declared(effect);
    // The host's arrays always hold 64 entries: every one is read here.
    // SAFETY: that contract.
    let (in_rows, out_rows) = unsafe { (std::slice::from_raw_parts(inputs, 64), std::slice::from_raw_parts(outputs, 64)) };
    if in_rows.iter().chain(out_rows).any(|row| row.is_null()) {
        probe.rows_missing.store(true, Relaxed);
        return;
    }
    if grown != 0 {
        for &row in &in_rows[before.0.min(ins)..ins] {
            probe.grown_inputs_read.fetch_add(1, Relaxed);
            // SAFETY: the host's row for an input declared a moment ago: at least one frame.
            if unsafe { *row } != 0.0 {
                probe.grown_inputs_loud.fetch_add(1, Relaxed);
            }
        }
        for &row in &out_rows[before.1.min(outs)..outs] {
            // SAFETY: the host's row for an output declared a moment ago: `frames` frames.
            unsafe { std::slice::from_raw_parts_mut(row, frames as usize) }.fill(1.0);
            probe.grown_outputs_filled.fetch_add(1, Relaxed);
        }
    }
    let distinct = |rows: &[*mut f32]| (0..rows.len()).filter(|&i| !rows[..i].contains(&rows[i])).count();
    probe.distinct_inputs.store(distinct(&in_rows[..ins.min(64)]), Relaxed);
    probe.distinct_outputs.store(distinct(&out_rows[..outs.min(64)]), Relaxed);
    // SAFETY: this live instance; neither opcode passes a pointer, and the time info the host
    // answers stays valid for this call.
    unsafe {
        let time = call_host(effect, AUDIO_MASTER_GET_TIME, 0, 0, 0.0) as *const VstTimeInfo;
        if let Some(time) = time.as_ref() {
            probe.time_pos.store(time.sample_pos.to_bits(), Relaxed);
            probe.time_rate.store(time.sample_rate.to_bits(), Relaxed);
            probe.time_tempo.store(time.tempo.to_bits(), Relaxed);
            probe.time_flags.store(time.flags, Relaxed);
        }
        probe.process_level.store(call_host(effect, AUDIO_MASTER_GET_CURRENT_PROCESS_LEVEL, 0, 0, 0.0), Relaxed);
    }
    let (level, spread) = (f32::from_bits(probe.level.load(Relaxed)), f32::from_bits(probe.spread.load(Relaxed)));
    let loud = match probe.signal_outputs.load(Relaxed) {
        0 => usize::MAX,
        some => some as usize,
    };
    for (index, &row) in out_rows.iter().enumerate().take(outs) {
        // SAFETY: a declared output's row holds `frames` frames; an input's of the same index too.
        let out = unsafe { std::slice::from_raw_parts_mut(row, frames as usize) };
        for (i, sample) in out.iter_mut().enumerate() {
            let wet = if ins > 0 {
                // SAFETY: as above.
                if index < ins { unsafe { *in_rows[index].add(i) } } else { 0.0 }
            } else if index < loud {
                match index {
                    0 => level + spread,
                    1 => level - spread,
                    _ => level,
                }
            } else {
                0.0
            };
            *sample = if add { *sample + wet } else { wet };
        }
    }
    probe.last_frames.store(frames, Relaxed);
    probe.processes.fetch_add(1, Relaxed);
    let grow = probe.grow_in_process.swap(0, Relaxed);
    if grow != 0 || grown != 0 {
        // SAFETY: this live instance changes its own layout and tells its host from inside the
        // process call, as a plugin does; the opcode passes no pointer.
        unsafe {
            if grow != 0 {
                (*effect).num_outputs = grow;
            }
            probe.awaiting_cycle.store(true, SeqCst);
            call_host(effect, AUDIO_MASTER_IO_CHANGED, 0, 0, 0.0);
        }
    }
}

unsafe extern "C" fn set_parameter(effect: *mut AEffect, index: i32, value: f32) {
    // SAFETY: the host passes the effect this fixture made.
    let Some(probe) = (unsafe { (*effect.cast::<Instance>()).shape.probe }) else {
        PARAMETER_CALLS.fetch_add(1, Relaxed);
        return;
    };
    probe.rt_call();
    if Probe::on_device() {
        probe.sets_on_device.fetch_add(1, Relaxed);
    }
    if let Some(stored) = usize::try_from(index).ok().and_then(|i| probe.values.get(i)) {
        stored.store(value.to_bits(), Relaxed);
    }
    probe.last_set_index.store(index, Relaxed);
    probe.last_set_value.store(value.to_bits(), Relaxed);
    probe.sets.fetch_add(1, Relaxed);
    if probe.automate_in_set.load(Relaxed) {
        // SAFETY: this live instance reports its own parameter; the opcode passes no pointer.
        unsafe { call_host(effect, AUDIO_MASTER_AUTOMATE, index, 0, value * 0.5) };
    }
}

unsafe extern "C" fn get_parameter(effect: *mut AEffect, index: i32) -> f32 {
    // SAFETY: the host passes the effect this fixture made.
    let Some(probe) = (unsafe { (*effect.cast::<Instance>()).shape.probe }) else {
        PARAMETER_CALLS.fetch_add(1, Relaxed);
        return 0.0;
    };
    probe.gets.fetch_add(1, Relaxed);
    usize::try_from(index).ok().and_then(|i| probe.values.get(i)).map_or(0.0, |stored| f32::from_bits(stored.load(Relaxed)))
}
