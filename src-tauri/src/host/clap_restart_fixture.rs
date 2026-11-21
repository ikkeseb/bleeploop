//! A raw CLAP plugin with real activate/process callbacks, loaded by clack with the production
//! `LfHost`. Proves `clap_host_params.rescan` reaches the owner and, through its `clap.params`
//! extension, that a malformed parameter count is refused. The engine-mode tests load the same plugin
//! through `engine_slot` into a test device's engine, where a `host.request_restart` cycles it
//! (deactivate → activate → processing resumed, on the right threads). No audio device or GUI
//! required.
use super::*;
#[allow(unused_imports)]
use crate::host::clap::ProcessMono;
use std::time::Instant;
use clap_sys::{
    entry::clap_plugin_entry,
    events::{
        clap_event_header, clap_event_note, clap_event_param_value, CLAP_CORE_EVENT_SPACE_ID,
        CLAP_EVENT_NOTE_OFF, CLAP_EVENT_NOTE_ON, CLAP_EVENT_PARAM_VALUE,
    },
    ext::audio_ports::{clap_audio_port_info, clap_plugin_audio_ports, CLAP_EXT_AUDIO_PORTS},
    ext::gui::{clap_gui_resize_hints, clap_host_gui, clap_plugin_gui, clap_window, CLAP_EXT_GUI, CLAP_WINDOW_API_WIN32},
    ext::latency::{clap_plugin_latency, CLAP_EXT_LATENCY},
    ext::params::{
        clap_host_params, clap_param_info, clap_plugin_params, CLAP_EXT_PARAMS, CLAP_PARAM_IS_HIDDEN,
        CLAP_PARAM_IS_READONLY, CLAP_PARAM_RESCAN_VALUES,
    },
    ext::state::{clap_host_state, clap_plugin_state, CLAP_EXT_STATE},
    stream::{clap_istream, clap_ostream},
    factory::plugin_factory::clap_plugin_factory,
    host::clap_host,
    id::{clap_id, CLAP_INVALID_ID},
    plugin::{clap_plugin, clap_plugin_descriptor},
    process::{clap_process, clap_process_status, CLAP_PROCESS_CONTINUE, CLAP_PROCESS_ERROR},
    string_sizes::{CLAP_NAME_SIZE, CLAP_PATH_SIZE},
    version::CLAP_VERSION,
};
use std::cell::RefCell;
use std::ffi::{c_char, c_void, CStr};
use rtrb::RingBuffer;
use std::sync::atomic::{AtomicI32, AtomicPtr, AtomicU64, AtomicUsize};
use std::sync::Mutex;
use std::thread::ThreadId;

const FEATURES: [*const c_char; 2] = [c"instrument".as_ptr(), std::ptr::null()];
static DESCRIPTOR: clap_plugin_descriptor = clap_plugin_descriptor {
    clap_version: CLAP_VERSION,
    id: c"bleeploop.restart-fixture".as_ptr(),
    name: c"Restart fixture".as_ptr(),
    vendor: c"BleepLoop".as_ptr(),
    url: c"".as_ptr(),
    manual_url: c"".as_ptr(),
    support_url: c"".as_ptr(),
    version: c"1".as_ptr(),
    description: c"".as_ptr(),
    features: FEATURES.as_ptr(),
};

/// One fixture instance: the host it was created with and the thread that created it (its main
/// thread), plus what it records (`Observed`, shared so a test can watch an instance it did not
/// create and read it after `destroy`).
struct FixtureState {
    host: *const clap_host,
    owner: ThreadId,
    obs: Arc<Observed>,
}

impl std::ops::Deref for FixtureState {
    type Target = Observed;
    fn deref(&self) -> &Observed {
        &self.obs
    }
}

/// The thread every engine-mode test renders on (`engine_io::test_rig`).
const TEST_DEVICE_THREAD: &str = "lf-test-device";

thread_local! {
    /// What the next instances created on this thread record into, in order; a fresh `Observed` once
    /// it runs out. An engine-mode test sets it from the owner thread, just before that thread
    /// instantiates (a load that discards a refusing instance creates a second one).
    static OBSERVE: RefCell<Vec<Arc<Observed>>> = const { RefCell::new(Vec::new()) };
}

/// Every lifecycle call the plugin sees, with the thread it saw it on. `Mutex<Vec<ThreadId>>` on
/// the main-thread calls only (activate/deactivate, never on the RT path); the RT-side calls
/// (start/stop/process) record through atomics so the fixture allocates nothing under the
/// production RT alloc guard.
#[derive(Default)]
struct Observed {
    host: AtomicPtr<clap_host>,
    activations: AtomicUsize,
    deactivations: AtomicUsize,
    starts: AtomicUsize,
    stops: AtomicUsize,
    processes: AtomicUsize,
    active: AtomicBool,
    processing: AtomicBool,
    /// activate/deactivate on a thread other than the owner, start/stop/process ON the owner,
    /// process while inactive/not-processing, start while already processing — any is a violation.
    contract_violation: AtomicBool,
    main_thread_calls: Mutex<Vec<ThreadId>>,
    /// What `clap.params` reports as its count; `get_info` answers only the first 16 indices, so
    /// a malformed count stays cheap to walk.
    param_count: AtomicU32,
    /// The `clap_param_info.flags` of each of those 16.
    param_flags: [AtomicU32; 16],
    /// What `clap.latency` reports.
    latency: AtomicU32,
    /// The input port's channel count (`clap.audio-ports`); 0 = no input port, an instrument. With
    /// an input the plugin is an effect that echoes each input channel to its output.
    inputs: AtomicU32,
    /// The output port's channel count (`clap.audio-ports`); 0 = default stereo (2).
    outputs: AtomicU32,
    /// Two ports each way instead (`TWO_PORT_*`): a stereo main and a mono sidechain in, a stereo
    /// main and a mono aux out. `process` echoes the main input, writes `AUX_MARKER` to the aux
    /// output and records whether the sidechain carried anything.
    two_ports: AtomicBool,
    /// The port counts the last `process` call received, per direction.
    seen_inputs: AtomicU32,
    seen_outputs: AtomicU32,
    /// A `process` call found a sample other than 0 on the sidechain.
    sidechain_heard: AtomicBool,
    /// `process` writes its outputs as usual, then returns `CLAP_PROCESS_ERROR`.
    fail_process: AtomicBool,
    /// The last activation's sample rate (f64 bits) and max frame count.
    activated_rate: AtomicU64,
    activated_max_frames: AtomicU32,
    /// Written to every output channel on each process call (f32 bits; 0 leaves them silent).
    output_level: AtomicU32,
    /// start/stop/process calls on a thread other than the test device's (engine-mode tests).
    rt_calls_off_device: AtomicUsize,
    note_ons: AtomicUsize,
    note_offs: AtomicUsize,
    last_key: AtomicI32,
    last_velocity: AtomicU64,
    last_note_time: AtomicU32,
    param_values: AtomicUsize,
    last_param: AtomicU32,
    last_param_value: AtomicU64,
    /// Lifecycle order: each of these takes the next `seq` value when its call happens.
    seq: AtomicUsize,
    stop_seq: AtomicUsize,
    deactivate_seq: AtomicUsize,
    destroy_seq: AtomicUsize,
    /// The tone tests (`clap.state`): the plugin's state, which `save` writes and `load` replaces.
    saved: Mutex<Vec<u8>>,
    /// `load` refuses whatever it is given.
    refuse_state: AtomicBool,
    /// `load` applies what it is given, then refuses: a plugin that takes the fields it knows before
    /// it meets one it does not.
    half_apply_state: AtomicBool,
    /// `save` writes 1 MiB at a time until the host's stream refuses a write (a sampler, or a runaway
    /// plugin), ignoring the refusal once and writing on; `save_accepted` counts what the host took.
    flood_save: AtomicBool,
    save_accepted: AtomicUsize,
    /// The next main-thread callback opens a window whose procedure posts itself another message
    /// for every one it handles, forever (`flood_window`).
    flood_messages: AtomicBool,
    state_loads: AtomicUsize,
    /// A `load` arrived while the plugin was active: a tone must go in before activation.
    loaded_while_active: AtomicBool,
    /// The next `process` reports a parameter change on its output events, as an editor knob does.
    emit_edit: AtomicBool,
    /// The next main-thread callback calls the host's `clap_host_state.mark_dirty`.
    dirty_on_callback: AtomicBool,
    /// The next main-thread callback calls the host's `request_restart`.
    restart_on_callback: AtomicBool,
    /// Armed by a test: the next `activate` tells it it is there, then waits (≤ 5 s) for its go.
    #[allow(clippy::type_complexity)]
    park_in_activate: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
    /// Runs once at the end of the next `save`, its state written, on the thread that called it.
    #[allow(clippy::type_complexity)]
    on_save: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    marked_dirty: AtomicUsize,
    /// `clap.gui`: offered (WIN32 only) when either is set, as a floating window or embedded.
    gui_floating: AtomicBool,
    gui_embedded: AtomicBool,
    /// Every GUI call the host made that changes the GUI, in order (main-thread calls only).
    gui_calls: Mutex<Vec<&'static str>>,
    /// The window an embedded GUI was given.
    gui_parent: AtomicIsize,
}

impl Observed {
    fn tick(&self, at: &AtomicUsize) {
        at.store(self.seq.fetch_add(1, Relaxed) + 1, Relaxed);
    }
    fn off_device(&self) {
        if std::thread::current().name() != Some(TEST_DEVICE_THREAD) {
            self.rt_calls_off_device.fetch_add(1, Relaxed);
        }
    }
}

unsafe fn state<'a>(plugin: *const clap_plugin) -> &'a FixtureState {
    // SAFETY: every fixture plugin owns this boxed state until its destroy callback.
    unsafe { &*((*plugin).plugin_data.cast::<FixtureState>()) }
}

unsafe extern "C" fn init(_: *const clap_plugin) -> bool {
    true
}

unsafe extern "C" fn activate(plugin: *const clap_plugin, rate: f64, _: u32, max_frames: u32) -> bool {
    let s = unsafe { state(plugin) };
    if s.owner != std::thread::current().id() || s.active.swap(true, Relaxed) {
        s.contract_violation.store(true, Relaxed);
    }
    s.activated_rate.store(rate.to_bits(), Relaxed);
    s.activated_max_frames.store(max_frames, Relaxed);
    s.activations.fetch_add(1, Relaxed);
    s.main_thread_calls.lock().unwrap().push(std::thread::current().id());
    let parked = s.park_in_activate.lock().unwrap().take();
    if let Some((there, go)) = parked {
        let _ = there.send(());
        let _ = go.recv_timeout(Duration::from_secs(5));
    }
    true
}

unsafe extern "C" fn deactivate(plugin: *const clap_plugin) {
    let s = unsafe { state(plugin) };
    if s.owner != std::thread::current().id()
        || !s.active.swap(false, Relaxed)
        || s.processing.load(Relaxed)
    {
        s.contract_violation.store(true, Relaxed);
    }
    s.tick(&s.deactivate_seq);
    s.deactivations.fetch_add(1, Relaxed);
    s.main_thread_calls.lock().unwrap().push(std::thread::current().id());
}

unsafe extern "C" fn start_processing(plugin: *const clap_plugin) -> bool {
    let s = unsafe { state(plugin) };
    if s.owner == std::thread::current().id()
        || !s.active.load(Relaxed)
        || s.processing.swap(true, Relaxed)
    {
        s.contract_violation.store(true, Relaxed);
    }
    s.off_device();
    s.starts.fetch_add(1, Relaxed);
    true
}

unsafe extern "C" fn stop_processing(plugin: *const clap_plugin) {
    let s = unsafe { state(plugin) };
    if s.owner == std::thread::current().id() || !s.processing.swap(false, Relaxed) {
        s.contract_violation.store(true, Relaxed);
    }
    s.off_device();
    s.tick(&s.stop_seq);
    s.stops.fetch_add(1, Relaxed);
}

unsafe extern "C" fn process(
    plugin: *const clap_plugin,
    process: *const clap_process,
) -> clap_process_status {
    let s = unsafe { state(plugin) };
    if s.owner == std::thread::current().id()
        || !s.active.load(Relaxed)
        || !s.processing.load(Relaxed)
    {
        s.contract_violation.store(true, Relaxed);
    }
    s.off_device();
    // SAFETY: the host passes a valid clap_process whose event list and output buffers live for
    // this call; the fixture reads events through the list's own functions and writes at most
    // `frames_count` frames into each output channel it was given.
    unsafe {
        let p = &*process;
        let list = &*p.in_events;
        if let (Some(size), Some(get)) = (list.size, list.get) {
            for i in 0..size(list) {
                let header: &clap_event_header = &*get(list, i);
                if header.space_id != CLAP_CORE_EVENT_SPACE_ID {
                    continue;
                }
                match header.type_ {
                    CLAP_EVENT_NOTE_ON => {
                        let note = &*(header as *const clap_event_header).cast::<clap_event_note>();
                        s.last_key.store(note.key as i32, Relaxed);
                        s.last_velocity.store(note.velocity.to_bits(), Relaxed);
                        s.last_note_time.store(header.time, Relaxed);
                        s.note_ons.fetch_add(1, Relaxed);
                    }
                    CLAP_EVENT_NOTE_OFF => {
                        s.note_offs.fetch_add(1, Relaxed);
                    }
                    CLAP_EVENT_PARAM_VALUE => {
                        let param = &*(header as *const clap_event_header).cast::<clap_event_param_value>();
                        s.last_param.store(param.param_id, Relaxed);
                        s.last_param_value.store(param.value.to_bits(), Relaxed);
                        s.param_values.fetch_add(1, Relaxed);
                    }
                    _ => {}
                }
            }
        }
        if s.emit_edit.swap(false, Relaxed) {
            let edit = clap_event_param_value {
                header: clap_event_header {
                    size: std::mem::size_of::<clap_event_param_value>() as u32,
                    time: 0,
                    space_id: CLAP_CORE_EVENT_SPACE_ID,
                    type_: CLAP_EVENT_PARAM_VALUE,
                    flags: 0,
                },
                param_id: 100,
                cookie: std::ptr::null_mut(),
                note_id: -1,
                port_index: -1,
                channel: -1,
                key: -1,
                value: 0.9,
            };
            let out = &*p.out_events;
            if let Some(push) = out.try_push {
                push(out, &edit.header);
            }
        }
        s.seen_inputs.store(p.audio_inputs_count, Relaxed);
        s.seen_outputs.store(p.audio_outputs_count, Relaxed);
        // Only past both counts: a host that passes fewer ports is recorded above, never read past.
        if s.two_ports.load(Relaxed) && p.audio_inputs_count >= 2 && p.audio_outputs_count >= 2 {
            let (sidechain, aux) = (&*p.audio_inputs.add(1), &*p.audio_outputs.add(1));
            for c in 0..sidechain.channel_count as usize {
                let samples = std::slice::from_raw_parts(*sidechain.data32.add(c), p.frames_count as usize);
                if samples.iter().any(|&x| x != 0.0) {
                    s.sidechain_heard.store(true, Relaxed);
                }
            }
            for c in 0..aux.channel_count as usize {
                std::slice::from_raw_parts_mut(*aux.data32.add(c), p.frames_count as usize).fill(AUX_MARKER);
            }
        }
        let level = f32::from_bits(s.output_level.load(Relaxed));
        if p.audio_inputs_count > 0 && p.audio_outputs_count > 0 {
            let (input, output) = (&*p.audio_inputs, &*p.audio_outputs);
            for c in 0..input.channel_count.min(output.channel_count) as usize {
                let from = std::slice::from_raw_parts(*input.data32.add(c), p.frames_count as usize);
                std::slice::from_raw_parts_mut(*output.data32.add(c), p.frames_count as usize).copy_from_slice(from);
            }
        } else if level != 0.0 {
            for b in 0..p.audio_outputs_count as usize {
                let buffer = &*p.audio_outputs.add(b);
                for c in 0..buffer.channel_count as usize {
                    let channel = *buffer.data32.add(c);
                    std::slice::from_raw_parts_mut(channel, p.frames_count as usize).fill(level);
                }
            }
        }
    }
    s.processes.fetch_add(1, Relaxed);
    if s.fail_process.load(Relaxed) { CLAP_PROCESS_ERROR } else { CLAP_PROCESS_CONTINUE }
}

unsafe extern "C" fn reset(_: *const clap_plugin) {}

/// Messages `flood_window`'s procedure has handled.
static FLOODED: AtomicUsize = AtomicUsize::new(0);

/// A window procedure that answers each `WM_APP` by posting itself another: its thread's queue is
/// never empty again, as with a plugin whose timer or async update keeps rescheduling itself.
unsafe extern "system" fn flood_proc(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::{DefWindowProcW, PostMessageW, WM_APP};
    if msg == WM_APP {
        FLOODED.fetch_add(1, Relaxed);
        // SAFETY: posting to the window this procedure serves.
        let _ = unsafe { PostMessageW(Some(hwnd), WM_APP, wparam, lparam) };
        return windows::Win32::Foundation::LRESULT(0);
    }
    // SAFETY: the default procedure for everything else.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Open a message-only `flood_proc` window on this thread and post it its first message. It lives
/// until the thread ends.
fn flood_window() {
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, PostMessageW, RegisterClassW, HWND_MESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WNDCLASSW,
    };
    let class = windows::core::w!("BleepLoopFloodFixture");
    // SAFETY: a 'static class name and procedure; registering twice fails harmlessly.
    unsafe {
        RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(flood_proc), lpszClassName: class, ..Default::default() });
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            windows::core::w!(""),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        )
        .expect("a message-only window");
        PostMessageW(Some(hwnd), WM_APP, Default::default(), Default::default()).expect("the first message");
    }
}

/// The plugin's main-thread callback: marks its state dirty through the host, or starts a message
/// flood on its thread, when a test asked.
unsafe extern "C" fn on_main_thread(plugin: *const clap_plugin) {
    let s = unsafe { state(plugin) };
    if s.flood_messages.swap(false, Relaxed) {
        flood_window();
    }
    if s.restart_on_callback.swap(false, Relaxed) {
        // SAFETY: the host outlives its plugin; `request_restart` is thread-safe.
        unsafe {
            let host = &*s.host;
            (host.request_restart.unwrap())(host);
        }
    }
    if !s.dirty_on_callback.swap(false, Relaxed) {
        return;
    }
    // SAFETY: the host outlives its plugin; a non-null "clap.state" extension is a clap_host_state.
    unsafe {
        let host = &*s.host;
        let ext = host.get_extension.map_or(std::ptr::null(), |get| get(host, CLAP_EXT_STATE.as_ptr()));
        if let Some(mark_dirty) = ext.cast::<clap_host_state>().as_ref().and_then(|e| e.mark_dirty) {
            mark_dirty(host);
            s.marked_dirty.fetch_add(1, Relaxed);
        }
    }
}

/// `clap.state` save: the whole state in one write.
unsafe extern "C" fn state_save(plugin: *const clap_plugin, stream: *const clap_ostream) -> bool {
    let s = unsafe { state(plugin) };
    if s.flood_save.load(Relaxed) {
        static MIB: [u8; 1 << 20] = [0x5a; 1 << 20];
        // SAFETY: the host's stream is valid for this call.
        let (stream, mut refusals) = (unsafe { &*stream }, 0);
        let Some(write) = stream.write else { return false };
        // Bounded here (64 MiB) only so an unbounded host fails this test instead of the machine.
        while refusals < 2 && s.save_accepted.load(Relaxed) < 64 << 20 {
            let n = unsafe { write(stream, MIB.as_ptr().cast(), MIB.len() as u64) };
            if n < 0 {
                refusals += 1;
            } else {
                s.save_accepted.fetch_add(n as usize, Relaxed);
            }
        }
        return true; // as a plugin that ignores the failed writes would
    }
    let bytes = s.saved.lock().unwrap().clone();
    // SAFETY: the host's stream is valid for this call.
    let written = unsafe {
        let stream = &*stream;
        let Some(write) = stream.write else { return false };
        bytes.is_empty() || write(stream, bytes.as_ptr().cast(), bytes.len() as u64) == bytes.len() as i64
    };
    let hook = s.on_save.lock().unwrap().take();
    if let Some(hook) = hook {
        hook();
    }
    written
}

/// `clap.state` load: read to the end in small pieces, as a plugin reading field by field would.
unsafe extern "C" fn state_load(plugin: *const clap_plugin, stream: *const clap_istream) -> bool {
    let s = unsafe { state(plugin) };
    if s.owner != std::thread::current().id() {
        s.contract_violation.store(true, Relaxed);
    }
    s.loaded_while_active.fetch_or(s.active.load(Relaxed), Relaxed);
    let mut bytes = Vec::new();
    // SAFETY: the host's stream is valid for this call; `chunk` holds what each read asks for.
    unsafe {
        let stream = &*stream;
        let Some(read) = stream.read else { return false };
        let mut chunk = [0u8; 5];
        loop {
            let n = read(stream, chunk.as_mut_ptr().cast(), chunk.len() as u64);
            if n <= 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..n as usize]);
        }
    }
    if s.half_apply_state.load(Relaxed) {
        *s.saved.lock().unwrap() = bytes;
        return false;
    }
    if s.refuse_state.load(Relaxed) {
        return false;
    }
    *s.saved.lock().unwrap() = bytes;
    s.state_loads.fetch_add(1, Relaxed);
    true
}

static STATE: clap_plugin_state = clap_plugin_state { save: Some(state_save), load: Some(state_load) };

unsafe extern "C" fn destroy(plugin: *const clap_plugin) {
    let s = unsafe { state(plugin) };
    s.tick(&s.destroy_seq);
    unsafe {
        drop(Box::from_raw((*plugin).plugin_data.cast::<FixtureState>()));
        drop(Box::from_raw(plugin.cast_mut()));
    }
}

unsafe extern "C" fn param_count(plugin: *const clap_plugin) -> u32 {
    unsafe { state(plugin) }.param_count.load(Relaxed)
}

unsafe extern "C" fn param_info(
    plugin: *const clap_plugin,
    index: u32,
    info: *mut clap_param_info,
) -> bool {
    let s = unsafe { state(plugin) };
    if index >= s.param_count.load(Relaxed).min(16) {
        return false;
    }
    // SAFETY: the host passes a writable clap_param_info; every field is written.
    unsafe {
        info.write(clap_param_info {
            id: 100 + index,
            flags: s.param_flags[index as usize].load(Relaxed),
            cookie: std::ptr::null_mut(),
            name: [0; CLAP_NAME_SIZE],
            module: [0; CLAP_PATH_SIZE],
            min_value: 0.0,
            max_value: 1.0,
            default_value: 0.5,
        });
    }
    true
}

unsafe extern "C" fn param_value(_: *const clap_plugin, _: clap_id, out: *mut f64) -> bool {
    unsafe { out.write(0.25) };
    true
}

unsafe extern "C" fn latency(plugin: *const clap_plugin) -> u32 {
    unsafe { state(plugin) }.latency.load(Relaxed)
}

static LATENCY: clap_plugin_latency = clap_plugin_latency { get: Some(latency) };

/// The two-port layout's channel counts, per port (`Observed::two_ports`).
const TWO_PORT_INPUTS: [u32; 2] = [2, 1];
const TWO_PORT_OUTPUTS: [u32; 2] = [2, 1];
/// What the two-port layout writes to its aux output.
const AUX_MARKER: f32 = 0.25;

unsafe extern "C" fn port_count(plugin: *const clap_plugin, is_input: bool) -> u32 {
    let s = unsafe { state(plugin) };
    if s.two_ports.load(Relaxed) {
        2
    } else if is_input {
        (s.inputs.load(Relaxed) > 0) as u32
    } else {
        1
    }
}

unsafe extern "C" fn port_get(
    plugin: *const clap_plugin,
    index: u32,
    is_input: bool,
    info: *mut clap_audio_port_info,
) -> bool {
    let s = unsafe { state(plugin) };
    let inputs = s.inputs.load(Relaxed);
    let channel_count = if s.two_ports.load(Relaxed) {
        let layout = if is_input { TWO_PORT_INPUTS } else { TWO_PORT_OUTPUTS };
        match layout.get(index as usize) {
            Some(&channels) => channels,
            None => return false,
        }
    } else if index != 0 || (is_input && inputs == 0) {
        return false;
    } else if is_input {
        inputs
    } else {
        match s.outputs.load(Relaxed) {
            0 => 2,
            channels => channels,
        }
    };
    // SAFETY: the host passes a writable clap_audio_port_info; every field is written.
    unsafe {
        info.write(clap_audio_port_info {
            id: 2 * index + is_input as u32,
            name: [0; CLAP_NAME_SIZE],
            flags: 0,
            channel_count,
            port_type: std::ptr::null(),
            in_place_pair: CLAP_INVALID_ID,
        });
    }
    true
}

static AUDIO_PORTS: clap_plugin_audio_ports = clap_plugin_audio_ports { count: Some(port_count), get: Some(port_get) };

unsafe extern "C" fn gui_supported(plugin: *const clap_plugin, api: *const c_char, is_floating: bool) -> bool {
    let s = unsafe { state(plugin) };
    let offered = if is_floating { &s.gui_floating } else { &s.gui_embedded };
    !api.is_null() && unsafe { CStr::from_ptr(api) } == CLAP_WINDOW_API_WIN32 && offered.load(Relaxed)
}
unsafe extern "C" fn gui_preferred(_: *const clap_plugin, _: *mut *const c_char, _: *mut bool) -> bool {
    false
}
fn gui_call(plugin: *const clap_plugin, call: &'static str) -> bool {
    unsafe { state(plugin) }.gui_calls.lock().unwrap().push(call);
    true
}
unsafe extern "C" fn gui_create(plugin: *const clap_plugin, _: *const c_char, _: bool) -> bool {
    gui_call(plugin, "create")
}
unsafe extern "C" fn gui_destroy(plugin: *const clap_plugin) {
    gui_call(plugin, "destroy");
}
unsafe extern "C" fn gui_set_scale(_: *const clap_plugin, _: f64) -> bool {
    false
}
unsafe extern "C" fn gui_get_size(_: *const clap_plugin, width: *mut u32, height: *mut u32) -> bool {
    unsafe { (width.write(320), height.write(200)) };
    true
}
unsafe extern "C" fn gui_can_resize(_: *const clap_plugin) -> bool {
    false
}
unsafe extern "C" fn gui_resize_hints(_: *const clap_plugin, _: *mut clap_gui_resize_hints) -> bool {
    false
}
unsafe extern "C" fn gui_adjust_size(_: *const clap_plugin, _: *mut u32, _: *mut u32) -> bool {
    false
}
unsafe extern "C" fn gui_set_size(plugin: *const clap_plugin, _: u32, _: u32) -> bool {
    gui_call(plugin, "set_size")
}
unsafe extern "C" fn gui_set_parent(plugin: *const clap_plugin, window: *const clap_window) -> bool {
    // SAFETY: the host passes a WIN32 window, the only API this GUI offers.
    let hwnd = unsafe { (*window).specific.win32 } as isize;
    unsafe { state(plugin) }.gui_parent.store(hwnd, Relaxed);
    gui_call(plugin, "set_parent")
}
unsafe extern "C" fn gui_set_transient(_: *const clap_plugin, _: *const clap_window) -> bool {
    true
}
unsafe extern "C" fn gui_suggest_title(_: *const clap_plugin, _: *const c_char) {}
unsafe extern "C" fn gui_show(plugin: *const clap_plugin) -> bool {
    gui_call(plugin, "show")
}
unsafe extern "C" fn gui_hide(plugin: *const clap_plugin) -> bool {
    gui_call(plugin, "hide")
}

static GUI: clap_plugin_gui = clap_plugin_gui {
    is_api_supported: Some(gui_supported),
    get_preferred_api: Some(gui_preferred),
    create: Some(gui_create),
    destroy: Some(gui_destroy),
    set_scale: Some(gui_set_scale),
    get_size: Some(gui_get_size),
    can_resize: Some(gui_can_resize),
    get_resize_hints: Some(gui_resize_hints),
    adjust_size: Some(gui_adjust_size),
    set_size: Some(gui_set_size),
    set_parent: Some(gui_set_parent),
    set_transient: Some(gui_set_transient),
    suggest_title: Some(gui_suggest_title),
    show: Some(gui_show),
    hide: Some(gui_hide),
};

static PARAMS: clap_plugin_params = clap_plugin_params {
    count: Some(param_count),
    get_info: Some(param_info),
    get_value: Some(param_value),
    value_to_text: None,
    text_to_value: None,
    flush: None,
};

unsafe extern "C" fn extension(plugin: *const clap_plugin, id: *const c_char) -> *const c_void {
    let id = unsafe { CStr::from_ptr(id) };
    if id == CLAP_EXT_GUI {
        let s = unsafe { state(plugin) };
        let offered = s.gui_floating.load(Relaxed) || s.gui_embedded.load(Relaxed);
        if offered { (&GUI as *const clap_plugin_gui).cast() } else { std::ptr::null() }
    } else if id == CLAP_EXT_PARAMS {
        (&PARAMS as *const clap_plugin_params).cast()
    } else if id == CLAP_EXT_LATENCY {
        (&LATENCY as *const clap_plugin_latency).cast()
    } else if id == CLAP_EXT_AUDIO_PORTS {
        (&AUDIO_PORTS as *const clap_plugin_audio_ports).cast()
    } else if id == CLAP_EXT_STATE {
        (&STATE as *const clap_plugin_state).cast()
    } else {
        std::ptr::null()
    }
}

unsafe extern "C" fn create(
    _: *const clap_plugin_factory,
    host: *const clap_host,
    id: *const c_char,
) -> *const clap_plugin {
    if unsafe { CStr::from_ptr(id) } != c"bleeploop.restart-fixture" {
        return std::ptr::null();
    }
    let obs = OBSERVE.with(|o| {
        let mut queue = o.borrow_mut();
        if queue.is_empty() { Arc::default() } else { queue.remove(0) }
    });
    obs.host.store(host.cast_mut(), Relaxed);
    let state = Box::into_raw(Box::new(FixtureState {
        host,
        owner: std::thread::current().id(),
        obs,
    }));
    Box::into_raw(Box::new(clap_plugin {
        desc: &DESCRIPTOR,
        plugin_data: state.cast(),
        init: Some(init),
        destroy: Some(destroy),
        activate: Some(activate),
        deactivate: Some(deactivate),
        start_processing: Some(start_processing),
        stop_processing: Some(stop_processing),
        reset: Some(reset),
        process: Some(process),
        get_extension: Some(extension),
        on_main_thread: Some(on_main_thread),
    }))
}

unsafe extern "C" fn count(_: *const clap_plugin_factory) -> u32 {
    1
}
unsafe extern "C" fn descriptor(
    _: *const clap_plugin_factory,
    index: u32,
) -> *const clap_plugin_descriptor {
    if index == 0 {
        &DESCRIPTOR
    } else {
        std::ptr::null()
    }
}
static FACTORY: clap_plugin_factory = clap_plugin_factory {
    get_plugin_count: Some(count),
    get_plugin_descriptor: Some(descriptor),
    create_plugin: Some(create),
};
unsafe extern "C" fn factory(id: *const c_char) -> *const c_void {
    if unsafe { CStr::from_ptr(id) } == c"clap.plugin-factory" {
        (&FACTORY as *const clap_plugin_factory).cast()
    } else {
        std::ptr::null()
    }
}
unsafe extern "C" fn entry_init(_: *const c_char) -> bool {
    true
}
unsafe extern "C" fn entry_deinit() {}
static ENTRY: clap_plugin_entry = clap_plugin_entry {
    clap_version: CLAP_VERSION,
    init: Some(entry_init),
    deinit: Some(entry_deinit),
    get_factory: Some(factory),
};

fn fixture_instance() -> PluginInstance<LfHost> {
    // SAFETY: the static entry and factory remain alive; clack owns each fixture instance, whose
    // callbacks obey the ABI.
    let entry =
        unsafe { PluginEntry::load_from_raw(&ENTRY, c"bleeploop-restart-fixture.clap") }.unwrap();
    let info = HostInfo::new("BleepLoop", "BleepLoop", "https://bleeploop.local", "0.1.0").unwrap();
    // The entry is a static; clack keeps the plugin alive through the instance, so dropping our
    // `PluginEntry` handle here is the same lifetime the callback fixture relies on.
    PluginInstance::<LfHost>::new(
        |_| LfShared {
            editor_closed: Arc::new(EditorClosed::default()),
            hosted_hwnd: Arc::new(AtomicIsize::new(0)),
            pending_resize: AtomicU64::new(NO_RESIZE),
            callback_requested: AtomicBool::new(false),
            restart_requested: AtomicBool::new(false),
            keeps_tone: false,
        },
        |_| LfMain::default(),
        &entry,
        c"bleeploop.restart-fixture",
        &info,
    )
    .unwrap()
}

/// How long a test waits for what it expects before it fails. Far past a healthy run, since a wait
/// ends the moment its condition holds: a loaded CI runner starves no test (a 2 s bound did). Still a
/// bound, so a bug fails the test instead of hanging it.
const PATIENCE: Duration = Duration::from_secs(30);

/// Poll until `pred` holds or PATIENCE runs out; returns whether it held. A timeout names the
/// waiting line on stderr, which a failed test prints beside its assertion.
#[track_caller]
fn wait_for(pred: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + PATIENCE;
    while !pred() {
        if Instant::now() >= deadline {
            eprintln!("{}: still waiting after {PATIENCE:?}, giving up", std::panic::Location::caller());
            return false;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    true
}

#[test]
fn params_rescan_from_the_plugin_reaches_the_owner_once_per_request() {
    let mut instance = fixture_instance();
    let plugin = instance.raw_instance() as *const clap_plugin;
    let s = unsafe { state(plugin) };
    let host = unsafe { &*s.host };
    // The plugin resolves the host's `clap.params` extension through the real ABI…
    let ext = unsafe { (host.get_extension.unwrap())(host, CLAP_EXT_PARAMS.as_ptr()) }
        .cast::<clap_host_params>();
    assert!(!ext.is_null(), "the host must declare clap_host_params");
    assert!(!take_params_rescan(&mut instance), "nothing pending before a request");
    // …and reports a value rescan (a preset loaded in its own GUI), on the main thread.
    unsafe { ((*ext).rescan.unwrap())(host, CLAP_PARAM_RESCAN_VALUES) };
    unsafe { ((*ext).rescan.unwrap())(host, CLAP_PARAM_RESCAN_VALUES) };
    assert!(take_params_rescan(&mut instance), "one owner turn sees the request");
    assert!(!take_params_rescan(&mut instance), "a burst coalesces into one re-list");
    // request_flush is accepted (continuous processing is the flush) and must not panic.
    unsafe { ((*ext).request_flush.unwrap())(host) };
}

/// A plugin that reports a count no real plugin has gets an error, not an allocation sized from it;
/// a sane count still lists every parameter, live values included.
#[test]
fn a_malformed_param_count_is_an_error_not_an_abort() {
    let mut instance = fixture_instance();
    let s = unsafe { state(instance.raw_instance() as *const clap_plugin) };
    for bogus in [u32::MAX, 1 << 20] {
        s.param_count.store(bogus, Relaxed);
        let err = clap_param_descs(&mut instance).expect_err("a malformed count must be refused");
        assert!(err.contains("parameter count"), "{bogus}: {err}");
    }
    s.param_count.store(3, Relaxed);
    let params = clap_param_descs(&mut instance).expect("a sane count lists");
    assert_eq!(params.iter().map(|p| p.id).collect::<Vec<_>>(), vec![100, 101, 102]);
    assert!(params.iter().all(|p| p.default_value == 0.5 && p.value == 0.25));
}

// ── engine mode: the same plugin as a unit inside a test device's engine (`engine_slot`) ────────

mod engine {
    use super::super::engine_slot::{self, EngineSlotEvent, EngineSlotHandle, GaveUp, PluginFormat};
    #[allow(unused_imports)]
    use crate::host::clap::ProcessMono;
    use super::super::super::state::ToneRestore;
    use super::super::super::tone::{self, TempDir, ToneBinding, ToneIdentity};
    use super::*;
    use crate::engine_io::test_rig::TestDevice;
    use lf_engine::{Command, NoteTarget, SlotKind, TimedCommand};

    /// A device rendering 256-frame blocks at `rate`, with a constant 0.1 as its input, paced near
    /// real time.
    fn device(rate: u32) -> TestDevice {
        TestDevice::start(rate, 256, Duration::from_millis(5), |_| 0.1)
    }

    /// Load the fixture into `slot` through the engine-mode owner; `obs` watches the instance the
    /// owner thread creates. The sink keeps every event.
    fn load(device: &TestDevice, slot: usize, obs: &Arc<Observed>) -> (EngineSlotHandle, Arc<Mutex<Vec<EngineSlotEvent>>>) {
        load_with_tone(device, slot, obs, None)
    }

    fn load_with_tone(
        device: &TestDevice,
        slot: usize,
        obs: &Arc<Observed>,
        tone: Option<ToneBinding>,
    ) -> (EngineSlotHandle, Arc<Mutex<Vec<EngineSlotEvent>>>) {
        load_observed(device, slot, vec![obs.clone()], tone)
    }

    /// As `load_with_tone`, with a watcher for each instance the owner creates, in order.
    fn load_observed(
        device: &TestDevice,
        slot: usize,
        obs: Vec<Arc<Observed>>,
        tone: Option<ToneBinding>,
    ) -> (EngineSlotHandle, Arc<Mutex<Vec<EngineSlotEvent>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink_seen = seen.clone();
        let handle = engine_slot::spawn(
            PluginFormat::Clap,
            device.host().slot(slot),
            0,
            Arc::new(move |e| sink_seen.lock().unwrap().push(e)),
            tone,
            owner(obs),
        )
        .expect("the fixture loads into the engine");
        (handle, seen)
    }

    /// The production owner over the fixture's entry, with a watcher for each instance it creates.
    fn owner(obs: Vec<Arc<Observed>>) -> impl FnOnce(engine_slot::OwnerCtx) -> Result<(), String> + Send + 'static {
        move |ctx| {
            super::super::clap_engine::run(ctx, "bleeploop.restart-fixture", move || {
                OBSERVE.with(|o| *o.borrow_mut() = obs);
                // SAFETY: as `fixture_instance`: a static entry whose callbacks obey the ABI.
                unsafe { PluginEntry::load_from_raw(&ENTRY, c"bleeploop-restart-fixture.clap") }
                    .map_err(|e| e.to_string())
            })
        }
    }

    fn installed(device: &TestDevice, slot: usize) -> Option<(SlotKind, lf_engine::grid::Frame)> {
        device.host().core.rt.lock().unwrap().engine.as_ref().unwrap().slot(slot)
    }

    fn send(device: &TestDevice, command: Command) {
        device.host().send(TimedCommand { frame: None, command }).unwrap();
    }

    fn rendered_above(device: &TestDevice, level: f32) -> bool {
        device.output.lock().unwrap().iter().rev().take(256).all(|&x| x > level)
    }

    fn rendered_near(device: &TestDevice, level: f32) -> bool {
        device.output.lock().unwrap().iter().rev().take(256).all(|&x| (x - level).abs() < 1e-3)
    }

    #[test]
    fn a_loaded_slot_renders_in_the_engine_and_unloads_in_order() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let obs = Arc::new(Observed::default());
        obs.output_level.store(0.25f32.to_bits(), Relaxed);
        obs.latency.store(64, Relaxed);
        let (handle, _) = load(&device, 0, &obs);
        assert_eq!(handle.kind(), SlotKind::Instrument, "no audio input: an instrument");
        assert_eq!(handle.name(), "Restart fixture");
        assert!(wait_for(|| obs.processes.load(Relaxed) > 8), "the engine processes the unit");
        assert!(wait_for(|| rendered_above(&device, 0.2)), "its output reaches the device");
        assert_eq!(
            installed(&device, 0),
            Some((SlotKind::Instrument, 64)),
            "installed with the latency the plugin reported once active"
        );
        assert_eq!(f64::from_bits(obs.activated_rate.load(Relaxed)), 48_000.0, "activated at the engine's rate");
        assert_eq!(obs.activated_max_frames.load(Relaxed), 4096, "with the engine's largest block as max frames");

        handle.unload().expect("the unit comes back");
        assert_eq!(installed(&device, 0), None, "the slot is empty");
        assert_eq!((obs.starts.load(Relaxed), obs.stops.load(Relaxed)), (1, 1));
        assert_eq!(obs.deactivations.load(Relaxed), 1);
        let (stop, deactivate, destroy) =
            (obs.stop_seq.load(Relaxed), obs.deactivate_seq.load(Relaxed), obs.destroy_seq.load(Relaxed));
        assert!(
            0 < stop && stop < deactivate && deactivate < destroy,
            "stop {stop} → deactivate {deactivate} → destroy {destroy}"
        );
        assert!(!obs.contract_violation.load(Relaxed));
        assert_eq!(obs.rt_calls_off_device.load(Relaxed), 0, "start, stop and process ran on the device thread");
    }

    /// A load its caller gave up on is undone by its owner: given up before the install, the unit
    /// never enters the engine; given up after it, the report finds nobody and the unit leaves the
    /// engine. Either way the plugin is deactivated and destroyed, and the slot takes the next load.
    #[test]
    fn a_load_its_caller_gave_up_on_is_undone_and_frees_the_slot() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        for gave_up in [GaveUp::Before, GaveUp::After] {
            let obs = Arc::new(Observed::default());
            let (ended, report) = engine_slot::spawn_given_up(device.host().slot(0), gave_up, owner(vec![obs.clone()]));
            assert_eq!(ended, Ok(()), "{gave_up:?}: the owner ends");
            if gave_up == GaveUp::Before {
                assert_eq!(report, Some(Err("plugin load cancelled".to_string())));
            }
            // A unit left in the engine would be engaged by now.
            assert!(device.wait_blocks(4));
            assert_eq!(installed(&device, 0), None, "{gave_up:?}: the slot is empty");
            assert_eq!((obs.activations.load(Relaxed), obs.deactivations.load(Relaxed)), (1, 1), "{gave_up:?}");
            assert_eq!(obs.starts.load(Relaxed), obs.stops.load(Relaxed), "{gave_up:?}: stopped if it started");
            let (deactivate, destroy) = (obs.deactivate_seq.load(Relaxed), obs.destroy_seq.load(Relaxed));
            assert!(0 < deactivate && deactivate < destroy, "{gave_up:?}: deactivate {deactivate} → destroy {destroy}");
            assert!(!obs.contract_violation.load(Relaxed), "{gave_up:?}");
        }
        let (handle, _) = load(&device, 0, &Arc::new(Observed::default()));
        handle.unload().expect("the next load takes the slot");
    }

    #[test]
    fn an_effect_slot_takes_the_live_input_and_its_wet_output_reaches_the_device() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let obs = Arc::new(Observed::default());
        obs.inputs.store(2, Relaxed);
        let (handle, _) = load(&device, 0, &obs);
        assert_eq!(handle.kind(), SlotKind::Effect, "an audio input: an effect");
        assert!(wait_for(|| obs.processes.load(Relaxed) > 8 && rendered_near(&device, 0.0)), "not live: silence");
        assert_eq!(installed(&device, 0), Some((SlotKind::Effect, 0)));
        send(&device, Command::SetSlotLive(0, true));
        assert!(
            wait_for(|| rendered_near(&device, 0.1)),
            "live: the device input, copied to both input channels and echoed, summed back to mono"
        );
        handle.unload().unwrap();
    }

    #[test]
    fn a_plugin_requested_restart_cycles_activation_on_the_owner_while_the_engine_renders() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let obs = Arc::new(Observed::default());
        let (handle, _) = load(&device, 0, &obs);
        assert!(wait_for(|| obs.processes.load(Relaxed) > 8));
        obs.latency.store(128, Relaxed);
        let blocks_before = device.blocks.load(Relaxed);
        let processed_before = obs.processes.load(Relaxed);

        // Through the real host ABI, from a thread of the plugin's own.
        let host = unsafe { *obs.host.load(Relaxed) };
        std::thread::spawn(move || unsafe { (host.request_restart.unwrap())(&host) }).join().unwrap();
        assert!(
            wait_for(|| obs.activations.load(Relaxed) == 2
                && obs.starts.load(Relaxed) == 2
                && obs.processes.load(Relaxed) > processed_before + 8),
            "deactivate → activate → reinstalled → processing again"
        );
        assert_eq!((obs.stops.load(Relaxed), obs.deactivations.load(Relaxed)), (1, 1));
        assert!(device.blocks.load(Relaxed) > blocks_before, "the device kept rendering through the restart");
        assert_eq!(
            device.host().core.counters.lock_misses.load(Relaxed),
            0,
            "the owner never held the engine while the device ran"
        );
        assert!(!obs.contract_violation.load(Relaxed), "activate/deactivate on the owner, start/stop/process off it");
        assert_eq!(obs.rt_calls_off_device.load(Relaxed), 0, "start, stop and process ran on the device thread");
        let owner = obs.main_thread_calls.lock().unwrap().clone();
        assert_eq!(owner.len(), 3, "activate, deactivate, activate");
        assert!(owner.iter().all(|t| *t == owner[0]) && owner[0] != std::thread::current().id());
        assert_eq!(installed(&device, 0), Some((SlotKind::Instrument, 128)), "reinstalled at the latency it reports now");
        handle.unload().unwrap();
    }

    #[test]
    fn a_note_sent_through_the_engine_reaches_the_plugin() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let obs = Arc::new(Observed::default());
        let (handle, _) = load(&device, 1, &obs);
        assert!(wait_for(|| obs.processes.load(Relaxed) > 4));
        send(&device, Command::SelectInstrument(NoteTarget::Slot(1)));
        send(&device, Command::NoteOn(60, 0.5));
        assert!(wait_for(|| obs.note_ons.load(Relaxed) == 1), "the note-on reaches the plugin");
        assert_eq!(obs.last_key.load(Relaxed), 60);
        assert_eq!(f64::from_bits(obs.last_velocity.load(Relaxed)), 0.5);
        send(&device, Command::NoteOff(60));
        assert!(wait_for(|| obs.note_offs.load(Relaxed) == 1), "and its note-off");
        handle.unload().unwrap();
    }

    #[test]
    fn a_param_set_through_the_handle_reaches_the_plugin_and_an_unlisted_id_is_refused() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let obs = Arc::new(Observed::default());
        obs.param_count.store(2, Relaxed);
        let (handle, _) = load(&device, 0, &obs);
        let listed: Vec<u32> = handle.list_params().unwrap().iter().map(|p| p.id).collect();
        assert_eq!(listed, vec![100, 101]);
        assert!(handle.set_param(7, 0.1).is_err(), "an id the plugin never listed");
        handle.set_param(101, 0.75).unwrap();
        assert!(wait_for(|| obs.param_values.load(Relaxed) == 1), "the param reaches the processor");
        assert_eq!(obs.last_param.load(Relaxed), 101);
        assert_eq!(f64::from_bits(obs.last_param_value.load(Relaxed)), 0.75);
        handle.unload().unwrap();
    }

    /// The host's `clap.gui`, as the plugin `obs` watches reaches it.
    fn host_gui(obs: &Observed) -> (*const clap_host, &'static clap_host_gui) {
        let host = obs.host.load(Relaxed).cast_const();
        // SAFETY: the host outlives its plugin, and declares `clap.gui` (a `clap_host_gui`).
        let gui = unsafe { ((*host).get_extension.unwrap())(host, CLAP_EXT_GUI.as_ptr()) }.cast::<clap_host_gui>();
        (host, unsafe { gui.as_ref() }.expect("the host declares clap.gui"))
    }

    /// The plugin reports its GUI destroyed (`closed(true)`), floating or embedded: the owner
    /// acknowledges with `destroy` alone (no hide, no resize first), closes the host window and
    /// tells the caller. Show and hide requests are refused, as the host does not act on them.
    #[test]
    fn a_gui_the_plugin_destroyed_is_acknowledged_with_destroy_alone_for_either_editor_form() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        for floating in [true, false] {
            let obs = Arc::new(Observed::default());
            if floating { &obs.gui_floating } else { &obs.gui_embedded }.store(true, Relaxed);
            let (handle, seen) = load(&device, 0, &obs);
            assert_eq!(handle.open_editor(), Ok(()), "floating: {floating}");
            let opened = if floating { vec!["create", "show"] } else { vec!["create", "set_parent", "show"] };
            assert_eq!(*obs.gui_calls.lock().unwrap(), opened);
            let (host, gui) = host_gui(&obs);
            // SAFETY: the host's own callbacks, called as a plugin calls them (any thread for these).
            unsafe {
                assert!(!(gui.request_show.unwrap())(host), "show refused");
                assert!(!(gui.request_hide.unwrap())(host), "hide refused");
                (gui.closed.unwrap())(host, true);
            }
            assert!(wait_for(|| seen.lock().unwrap().contains(&EngineSlotEvent::EditorClosed)), "the caller is told");
            assert_eq!(obs.gui_calls.lock().unwrap()[opened.len()..], ["destroy"], "floating: {floating}");
            if !floating {
                let parent = windows::Win32::Foundation::HWND(obs.gui_parent.load(Relaxed) as *mut c_void);
                // SAFETY: a window-handle query; a destroyed handle answers false.
                assert!(!unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(parent)) }.as_bool(), "the host window is gone");
            }
            assert_eq!(handle.close_editor(), Ok(()), "nothing left to close");
            assert_eq!(obs.gui_calls.lock().unwrap().len(), opened.len() + 1);
            handle.unload().unwrap();
        }
    }

    /// The plugin reports its GUI destroyed (`closed(true)`) after the owner's turn has looked for
    /// that, and the player's close or the unload comes before its next turn: that close
    /// acknowledges with `destroy` alone too, never a `hide` on a GUI that is gone. The owner is
    /// parked past its look: the plugin's main-thread callback (in the turn's `turn`, before `idle`)
    /// asks for a restart, which the turn cycles after `idle`, and its `activate` waits for the test.
    #[test]
    fn a_gui_destroyed_just_before_a_close_or_an_unload_gets_destroy_alone() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        for floating in [true, false] {
            for unload in [false, true] {
                let obs = Arc::new(Observed::default());
                if floating { &obs.gui_floating } else { &obs.gui_embedded }.store(true, Relaxed);
                let (handle, _) = load(&device, 0, &obs);
                assert_eq!(handle.open_editor(), Ok(()));
                let opened = obs.gui_calls.lock().unwrap().len();
                let (host, gui) = host_gui(&obs);
                let ((there, parked), (go, wait)) = (std::sync::mpsc::channel(), std::sync::mpsc::channel());
                *obs.park_in_activate.lock().unwrap() = Some((there, wait));
                obs.restart_on_callback.store(true, Relaxed);
                // SAFETY: the host's own callbacks, called as a plugin calls them (any thread).
                unsafe {
                    let raw = &*host;
                    (raw.request_callback.unwrap())(raw);
                }
                parked.recv_timeout(Duration::from_secs(5)).expect("the owner is parked past its turn's idle");
                // SAFETY: as above.
                unsafe { (gui.closed.unwrap())(host, true) };
                let (reply, answer) = std::sync::mpsc::sync_channel(1);
                if unload {
                    handle.stop_serving();
                } else {
                    handle.queue(OwnerRequest::CloseEditor(Arc::new(AtomicBool::new(false)), reply));
                }
                go.send(()).unwrap();
                if !unload {
                    assert_eq!(answer.recv_timeout(Duration::from_secs(5)), Ok(Ok(())), "the close is served");
                    assert_eq!(obs.gui_calls.lock().unwrap()[opened..], ["destroy"], "floating: {floating}, closed");
                }
                handle.unload().unwrap();
                assert_eq!(obs.gui_calls.lock().unwrap()[opened..], ["destroy"], "floating: {floating}, unload: {unload}");
                assert!(!obs.contract_violation.load(Relaxed));
            }
        }
    }

    /// A read-only parameter (a meter, a value the plugin reports) and a hidden one are neither
    /// listed nor settable: a host-set value would fight the plugin's own.
    #[test]
    fn read_only_and_hidden_params_are_not_listed_and_a_set_on_one_is_refused() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let obs = Arc::new(Observed::default());
        obs.param_count.store(3, Relaxed);
        obs.param_flags[0].store(CLAP_PARAM_IS_READONLY, Relaxed);
        obs.param_flags[2].store(CLAP_PARAM_IS_HIDDEN, Relaxed);
        let (handle, _) = load(&device, 0, &obs);
        let listed: Vec<u32> = handle.list_params().unwrap().iter().map(|p| p.id).collect();
        assert_eq!(listed, vec![101]);
        assert!(handle.set_param(100, 0.5).is_err(), "a read-only parameter");
        assert!(handle.set_param(102, 0.5).is_err(), "a hidden parameter");
        handle.set_param(101, 0.75).unwrap();
        assert!(wait_for(|| obs.param_values.load(Relaxed) == 1), "the writable one reaches the processor");
        assert_eq!(obs.last_param.load(Relaxed), 101);
        handle.unload().unwrap();
    }

    #[test]
    fn a_plugin_with_a_sidechain_and_an_aux_output_gets_every_port_and_the_slot_plays_its_main() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let obs = Arc::new(Observed::default());
        obs.two_ports.store(true, Relaxed);
        let (handle, _) = load(&device, 0, &obs);
        assert_eq!(handle.kind(), SlotKind::Effect, "a main input: an effect");
        send(&device, Command::SetSlotLive(0, true));
        assert!(
            wait_for(|| obs.processes.load(Relaxed) > 8 && rendered_near(&device, 0.1)),
            "live: the main input echoed; the aux output's marker never plays"
        );
        assert_eq!(
            (obs.seen_inputs.load(Relaxed), obs.seen_outputs.load(Relaxed)),
            (2, 2),
            "every declared port, both directions"
        );
        assert!(!obs.sidechain_heard.load(Relaxed), "the sidechain is silent");
        handle.unload().unwrap();
        assert!(!obs.contract_violation.load(Relaxed));
    }

    #[test]
    fn a_failed_process_call_plays_nothing_in_the_engine() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let obs = Arc::new(Observed::default());
        obs.output_level.store(0.25f32.to_bits(), Relaxed);
        obs.fail_process.store(true, Relaxed);
        let (handle, _) = load(&device, 0, &obs);
        assert!(wait_for(|| obs.processes.load(Relaxed) > 8));
        assert!(
            wait_for(|| rendered_near(&device, 0.0)),
            "the output of a failed call is discarded, though the plugin wrote it"
        );
        obs.fail_process.store(false, Relaxed);
        assert!(wait_for(|| rendered_above(&device, 0.2)), "the same output plays once the calls succeed");
        handle.unload().unwrap();
    }

    fn identity() -> ToneIdentity {
        ToneIdentity { format: "clap".into(), path: r"C:\fixture.clap".into(), id: "bleeploop.restart-fixture".into() }
    }

    /// The fixture's state as the store holds it.
    fn stored(dir: &TempDir) -> Option<Vec<u8>> {
        dir.binding(0, identity()).store.load(0, &identity()).unwrap().map(|t| t.state)
    }

    fn store_tone(dir: &TempDir, state: &[u8]) {
        let t = tone::Tone { identity: identity(), name: "Restart fixture".into(), state: state.to_vec() };
        dir.binding(0, identity()).store.save_encoded(0, &tone::encode(&t)).unwrap();
    }

    #[test]
    fn a_stored_tone_goes_in_before_activation_and_saves_follow_the_plugins_changes() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let dir = TempDir::new("clap-engine");
        store_tone(&dir, b"state v");
        let obs = Arc::new(Observed::default());
        let (handle, _) = load_with_tone(&device, 0, &obs, Some(dir.binding(0, identity())));
        assert_eq!(handle.tone(), Some(ToneRestore::Restored));
        assert_eq!(*obs.saved.lock().unwrap(), b"state v", "the plugin read the whole state");
        assert!(!obs.loaded_while_active.load(Relaxed), "the tone went in before activate");
        assert!(wait_for(|| obs.processes.load(Relaxed) > 4), "and the plugin runs");

        // An export takes the tone fresh; it lands in the store too.
        *obs.saved.lock().unwrap() = b"state w".to_vec();
        let file = tone::decode(&handle.take_tone().unwrap()).unwrap();
        assert_eq!((file.identity, file.name.as_str(), file.state), (identity(), "Restart fixture", b"state w".to_vec()));
        assert_eq!(stored(&dir).as_deref(), Some(&b"state w"[..]));

        // A change the plugin reports on its output events (its editor, on the audio thread): saved once
        // it has been quiet for SAVE_QUIET.
        *obs.saved.lock().unwrap() = b"state x".to_vec();
        obs.emit_edit.store(true, Relaxed);
        assert!(wait_for(|| stored(&dir).as_deref() == Some(&b"state x"[..])), "debounced save after an edit");

        // `mark_dirty` from the plugin's main thread; an unload saves it at once.
        *obs.saved.lock().unwrap() = b"state y".to_vec();
        obs.dirty_on_callback.store(true, Relaxed);
        let host = obs.host.load(Relaxed);
        // SAFETY: the host lives while the plugin is loaded; `request_callback` is thread-safe.
        unsafe { ((*host).request_callback.unwrap())(host) };
        assert!(wait_for(|| obs.marked_dirty.load(Relaxed) == 1), "the host declares clap.state");
        handle.unload().unwrap();
        assert_eq!(stored(&dir).as_deref(), Some(&b"state y"[..]), "saved before the teardown");
        assert!(!obs.contract_violation.load(Relaxed), "state calls on the owner thread");
    }

    /// A change the plugin reports after the owner loop's last turn (a `mark_dirty` from a
    /// main-thread callback it asked for once the unload had begun): the unload delivers the
    /// callback, collects the change and stores the tone the plugin has then, its bytes exactly.
    #[test]
    fn a_change_reported_after_the_last_owner_turn_is_in_the_tone_the_unload_stores() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let dir = TempDir::new("clap-late-change");
        let obs = Arc::new(Observed::default());
        let (handle, _) = load_with_tone(&device, 0, &obs, Some(dir.binding(0, identity())));
        assert!(wait_for(|| obs.processes.load(Relaxed) > 4));
        *obs.saved.lock().unwrap() = b"state v".to_vec();

        // Hold the owner inside an export's save, past this turn's delivery of callbacks.
        let (inside, entered) = std::sync::mpsc::channel();
        let (release, go) = std::sync::mpsc::channel::<()>();
        *obs.on_save.lock().unwrap() = Some(Box::new(move || {
            inside.send(()).unwrap();
            let _ = go.recv_timeout(PATIENCE);
        }));
        let export = handle.start_tone_save().unwrap();
        entered.recv_timeout(PATIENCE).expect("the owner is inside the save");
        *obs.saved.lock().unwrap() = b"state w".to_vec();
        obs.dirty_on_callback.store(true, Relaxed);
        let host = obs.host.load(Relaxed);
        // SAFETY: the host lives while the plugin is loaded; `request_callback` is thread-safe.
        unsafe { ((*host).request_callback.unwrap())(host) };
        // The unload is asked for before the owner gets on: its loop ends without another turn.
        handle.stop_serving();
        release.send(()).unwrap();
        let export = tone::decode(&export.recv_timeout(PATIENCE).unwrap().unwrap()).unwrap();
        assert_eq!(export.state, b"state v", "the export took the state before the change");
        handle.unload().unwrap();
        assert_eq!(obs.marked_dirty.load(Relaxed), 1, "the teardown delivered the callback");
        assert_eq!(stored(&dir).as_deref(), Some(&b"state w"[..]), "and the unload stored the changed tone");
        assert!(!obs.contract_violation.load(Relaxed), "state calls on the owner thread");
    }

    #[test]
    fn an_import_stops_the_earlier_load_in_its_slot_saving_and_its_reload_restores_it() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let dir = TempDir::new("clap-import");
        store_tone(&dir, b"state v");
        // The plugin in both slots, loaded before a session import stores its tone for slot A.
        let (a, b) = (fresh(), fresh());
        let (slot_a, _) = load_with_tone(&device, 0, &a, Some(dir.binding(0, identity())));
        let (slot_b, _) = load_with_tone(&device, 1, &b, Some(dir.binding(1, identity())));
        let t = tone::Tone { identity: identity(), name: "Restart fixture".into(), state: b"imported".to_vec() };
        let imported = dir.store().import(0, &tone::encode(&t), &identity()).unwrap();

        // Both slots change and save on: none of it lands on the import, and slot B keeps its own tone.
        *a.saved.lock().unwrap() = b"slot a".to_vec();
        *b.saved.lock().unwrap() = b"slot b".to_vec();
        slot_a.set_param(101, 0.75).unwrap();
        slot_b.set_param(101, 0.25).unwrap();
        let exit = slot_b.start_tone_save().unwrap().recv_timeout(PATIENCE).unwrap().unwrap();
        assert_eq!(tone::decode(&exit).unwrap().state, b"slot b", "an export or the exit still gets what B plays");
        slot_a.unload().unwrap();
        assert_eq!(stored(&dir).as_deref(), Some(&b"imported"[..]), "the store keeps the import");

        // Slot A's reload is handed the import, and restores it even if the file changed since.
        store_tone(&dir, b"written since");
        let mut binding = dir.binding(0, identity());
        binding.imported = Some(imported);
        let reloaded = fresh();
        let (slot_a, _) = load_with_tone(&device, 0, &reloaded, Some(binding));
        assert_eq!(slot_a.tone(), Some(ToneRestore::Restored));
        assert_eq!(*reloaded.saved.lock().unwrap(), b"imported", "the reload plays the session's tone");
        *reloaded.saved.lock().unwrap() = b"after".to_vec();
        slot_a.set_param(101, 0.5).unwrap();
        slot_a.unload().unwrap();
        assert_eq!(stored(&dir).as_deref(), Some(&b"after"[..]), "a load from after the import saves as usual");
        slot_b.unload().unwrap();
        assert_eq!(stored(&dir).as_deref(), Some(&b"after"[..]), "slot B's load never writes slot A's tone");
        let slot_b_tone = dir.store().load(1, &identity()).unwrap().map(|t| t.state);
        assert_eq!(slot_b_tone.as_deref(), Some(&b"slot b"[..]), "it saves its own");
    }

    /// A watcher for an instance that refuses its tone as `how` does, and lists two params.
    fn refusing(how: fn(&Observed) -> &AtomicBool) -> Arc<Observed> {
        let obs = Arc::new(Observed::default());
        how(&obs).store(true, Relaxed);
        obs.param_count.store(2, Relaxed);
        obs
    }

    fn fresh() -> Arc<Observed> {
        let obs = Arc::new(Observed::default());
        obs.param_count.store(2, Relaxed);
        obs
    }

    #[test]
    fn a_refused_tone_stays_stored_through_the_exit_save_and_the_unload() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let dir = TempDir::new("clap-kept");
        store_tone(&dir, b"state v");
        let (first, second) = (refusing(|o| &o.refuse_state), fresh());
        let (handle, _) = load_observed(&device, 0, vec![first, second.clone()], Some(dir.binding(0, identity())));
        assert_eq!(handle.tone(), Some(ToneRestore::Failed), "refused, reported");
        assert!(wait_for(|| second.processes.load(Relaxed) > 4), "and the load went on");
        // The app's exit: a save asked of every slot, then the unload. Neither writes the defaults over
        // the tone the plugin refused (a reinstalled version of it may take it again).
        let exit = handle.start_tone_save().unwrap().recv_timeout(PATIENCE).unwrap().unwrap();
        assert_eq!(tone::decode(&exit).unwrap().state, b"", "the save still hands back what plays: the defaults");
        assert_eq!(stored(&dir).as_deref(), Some(&b"state v"[..]), "the exit's save leaves the stored tone alone");
        handle.unload().unwrap();
        assert_eq!(stored(&dir).as_deref(), Some(&b"state v"[..]), "so does the unload");

        // A change the player makes is what the defaults replace it for.
        let (first, second) = (refusing(|o| &o.refuse_state), fresh());
        let (handle, _) = load_observed(&device, 0, vec![first, second], Some(dir.binding(0, identity())));
        handle.set_param(101, 0.75).unwrap();
        handle.unload().unwrap();
        assert_eq!(stored(&dir).as_deref(), Some(&b""[..]), "a change is saved as usual");
    }

    #[test]
    fn a_tone_the_plugin_half_takes_then_refuses_leaves_a_fresh_instance_at_its_defaults() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let dir = TempDir::new("clap-half");
        store_tone(&dir, b"state v");
        let (first, second) = (refusing(|o| &o.half_apply_state), fresh());
        let (handle, _) = load_observed(&device, 0, vec![first.clone(), second.clone()], Some(dir.binding(0, identity())));
        assert_eq!(handle.tone(), Some(ToneRestore::Failed), "refused, reported");
        assert_eq!(first.activations.load(Relaxed), 0, "the instance that took part of the tone never ran");
        assert!(first.destroy_seq.load(Relaxed) > 0, "it was destroyed");
        assert_eq!(second.state_loads.load(Relaxed), 0, "a fresh instance never saw the tone");
        assert!(wait_for(|| second.processes.load(Relaxed) > 4), "and it runs");
        let taken = tone::decode(&handle.take_tone().unwrap()).unwrap();
        assert!(taken.state.is_empty(), "at its true defaults: {:?}", String::from_utf8_lossy(&taken.state));
        handle.unload().unwrap();
        assert_eq!(stored(&dir).as_deref(), Some(&b"state v"[..]), "the stored tone stays");
        assert!(!second.contract_violation.load(Relaxed));
    }

    #[test]
    fn a_state_past_the_limit_is_refused_as_it_is_written_and_never_stored() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let dir = TempDir::new("clap-flood");
        store_tone(&dir, b"state v");
        let obs = Arc::new(Observed::default());
        let (handle, _) = load_with_tone(&device, 0, &obs, Some(dir.binding(0, identity())));
        assert_eq!(handle.tone(), Some(ToneRestore::Restored));
        obs.flood_save.store(true, Relaxed);
        let taken = handle.take_tone();
        let accepted = obs.save_accepted.load(Relaxed);
        assert!(accepted <= tone::MAX_STATE_BYTES, "the host took {accepted} bytes, past the limit");
        assert!(taken.is_err(), "a state that did not fit is an error, even though the plugin said it saved");
        assert_eq!(stored(&dir).as_deref(), Some(&b"state v"[..]), "and nothing truncated reaches the store");
        obs.flood_save.store(false, Relaxed);
        handle.unload().unwrap();
    }

    #[test]
    fn a_plugin_flooding_its_thread_with_messages_still_gets_requests_and_unloads() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let obs = fresh();
        let (handle, _) = load(&device, 0, &obs);
        obs.flood_messages.store(true, Relaxed);
        let host = obs.host.load(Relaxed);
        // SAFETY: the host lives while the plugin is loaded; `request_callback` is thread-safe.
        unsafe { ((*host).request_callback.unwrap())(host) };
        let before = FLOODED.load(Relaxed);
        assert!(wait_for(|| FLOODED.load(Relaxed) > before + 1000), "the flood runs on the owner thread");
        let listed = handle.list_params().map(|params| params.len());
        // Off this thread, so a stuck owner fails the test instead of hanging it.
        let (unloaded_tx, unloaded) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = unloaded_tx.send(handle.unload());
        });
        let unloaded = unloaded.recv_timeout(PATIENCE);
        assert_eq!(listed, Ok(2), "a request is still answered");
        assert!(matches!(unloaded, Ok(Ok(()))), "and the unload finishes: {unloaded:?}");
    }

    #[test]
    fn an_eviction_reactivates_the_plugin_at_the_new_rate() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let obs = Arc::new(Observed::default());
        let (handle, _) = load(&device, 0, &obs);
        assert!(wait_for(|| obs.processes.load(Relaxed) > 4));

        device.rebuild_at(44_100, 1024);
        assert!(
            wait_for(|| obs.activations.load(Relaxed) == 2 && obs.starts.load(Relaxed) == 2),
            "the owner took the unit back and reinstalled it"
        );
        assert_eq!(f64::from_bits(obs.activated_rate.load(Relaxed)), 44_100.0, "re-activated at the new rate");
        assert_eq!(obs.activated_max_frames.load(Relaxed), 1024, "and the new engine's block");
        assert_eq!((obs.stops.load(Relaxed), obs.deactivations.load(Relaxed)), (1, 1));
        let processed = obs.processes.load(Relaxed);
        assert!(wait_for(|| obs.processes.load(Relaxed) > processed + 4), "the new engine processes it");
        assert!(installed(&device, 0).is_some());
        assert!(!obs.contract_violation.load(Relaxed));
        handle.unload().unwrap();
        assert_eq!(obs.deactivations.load(Relaxed), 2);
    }
}

/// The unit on its own: a call longer than the plugin's max frames goes in slices, each note lands
/// in its slice at its offset there, ring params go into the first, the outputs are summed to mono,
/// and nothing allocates once processing has started.
#[cfg(debug_assertions)]
#[test]
fn a_unit_slices_a_long_call_places_each_event_and_allocates_nothing() {
    use super::clap_engine::{ClapUnit, Terms};
    #[allow(unused_imports)]
    use crate::host::clap::ProcessMono;
    use lf_engine::{SlotEvent, SlotEventKind, UnitProcessor};

    let mut instance = fixture_instance();
    let s = unsafe { state(instance.raw_instance() as *const clap_plugin) };
    s.output_level.store(0.5f32.to_bits(), Relaxed);
    assert!(
        super::engine_slot::rt_allocations(|| drop(std::hint::black_box(vec![0u8; 16]))) > 0,
        "the guard counts an allocation (control)"
    );
    let config = PluginAudioConfiguration { sample_rate: 48_000.0, min_frames_count: 1, max_frames_count: 64 };
    let stopped = instance.activate(|_, _| (), config).unwrap();
    let (mut params, ring) = RingBuffer::<ParamEvent>::new(8);
    let faults = Arc::new(AtomicU32::new(0));
    let terms = Terms { rate: 48_000, max_frames: 64, inputs: vec![], outputs: vec![2], latency: 0 };
    let unit = ClapUnit::new(stopped, &terms, ring, faults.clone(), Arc::new(AtomicBool::new(false)));
    let (mut unit, runs) = std::thread::spawn(move || {
        let mut unit = unit;
        let (input, mut out) = ([0.0f32; 200], [0.0f32; 200]);
        unit.process_mono(0, &input[..64], &[], &mut out[..64]); // starts processing: one call
        params.push(ParamEvent { id: 100, value: 0.5 }).unwrap();
        let notes = [
            SlotEvent { offset: 10, kind: SlotEventKind::NoteOn { key: 60, velocity: 1.0 } },
            SlotEvent { offset: 150, kind: SlotEventKind::NoteOn { key: 64, velocity: 1.0 } },
        ];
        // A window the shared counter was reset in runs again (`rt_allocations`); count the runs.
        let mut runs = 0;
        let allocations = super::engine_slot::rt_allocations(|| {
            runs += 1;
            unit.process_mono(200, &input, &notes, &mut out)
        });
        assert_eq!(allocations, 0, "process allocates nothing");
        assert!(out.iter().all(|&x| x == 0.5), "both channels at 0.5, summed to mono");
        unit.stop();
        (unit, runs)
    })
    .join()
    .unwrap();
    assert_eq!(s.processes.load(Relaxed), 1 + 4 * runs, "200 frames at 64 max: 64 + 64 + 64 + 8");
    assert_eq!(s.note_ons.load(Relaxed), 2 * runs);
    assert_eq!(
        (s.last_key.load(Relaxed), s.last_note_time.load(Relaxed)),
        (64, 150 - 128),
        "the second note in the third slice"
    );
    assert_eq!(s.param_values.load(Relaxed), 1, "the ring's param once, in the first slice");
    assert_eq!(faults.load(Relaxed), 0);
    instance.deactivate(unit.take_stopped().unwrap());
    assert!(!s.contract_violation.load(Relaxed));
}

/// The fixture as a unit on its own at 48 kHz and 64 max frames, its ports read by the production
/// query (`query_ports`), as a load reads them.
fn bare_unit(instance: &mut PluginInstance<LfHost>, faults: &Arc<AtomicU32>) -> Box<super::clap_engine::ClapUnit> {
    use super::clap_engine::{ClapUnit, Terms};
    #[allow(unused_imports)]
    use crate::host::clap::ProcessMono;
    let (inputs, outputs) = (query_ports(instance, true).unwrap(), query_ports(instance, false).unwrap());
    let config = PluginAudioConfiguration { sample_rate: 48_000.0, min_frames_count: 1, max_frames_count: 64 };
    let stopped = instance.activate(|_, _| (), config).unwrap();
    let terms = Terms { rate: 48_000, max_frames: 64, inputs, outputs, latency: 0 };
    // No params in these tests: the ring's producer is dropped at once.
    let (_, ring) = RingBuffer::<ParamEvent>::new(8);
    ClapUnit::new(stopped, &terms, ring, faults.clone(), Arc::new(AtomicBool::new(false)))
}

/// A plugin with a sidechain input and an aux output gets every port it declares: the sidechain
/// silent, the main input echoed to the slot, the aux output never played, and nothing allocates, on
/// the first call (which starts processing) or on one sliced past the max frame count.
#[cfg(debug_assertions)]
#[test]
fn a_unit_passes_every_declared_port_and_plays_only_the_main_output() {
    use lf_engine::UnitProcessor;

    let mut instance = fixture_instance();
    let s = unsafe { state(instance.raw_instance() as *const clap_plugin) };
    s.two_ports.store(true, Relaxed);
    let faults = Arc::new(AtomicU32::new(0));
    let unit = bare_unit(&mut instance, &faults);
    let mut unit = std::thread::spawn(move || {
        let mut unit = unit;
        let input: [f32; 200] = std::array::from_fn(|i| (i + 1) as f32 / 1000.0);
        let mut out = [1.0f32; 200];
        let first = super::engine_slot::rt_allocations(|| unit.process_mono(0, &input[..64], &[], &mut out[..64]));
        assert_eq!(first, 0, "the first call allocates nothing");
        assert_eq!(out[..64], input[..64], "the main input echoed; the aux marker never plays");
        let sliced = super::engine_slot::rt_allocations(|| unit.process_mono(64, &input, &[], &mut out));
        assert_eq!(sliced, 0, "a sliced call allocates nothing");
        assert_eq!(out, input, "across every slice");
        unit.stop();
        unit
    })
    .join()
    .unwrap();
    assert_eq!((s.seen_inputs.load(Relaxed), s.seen_outputs.load(Relaxed)), (2, 2), "every declared port, both directions");
    assert!(!s.sidechain_heard.load(Relaxed), "the sidechain is silent");
    assert_eq!(faults.load(Relaxed), 0);
    instance.deactivate(unit.take_stopped().unwrap());
    assert!(!s.contract_violation.load(Relaxed));
}

/// `reactivate` at terms unlike the activation's (48 kHz, 64 frames) deactivates the plugin, activates
/// it at the new rate and block, and hands the unit back re-armed: one process call takes a whole
/// block of the new size. Refused terms (rate 0, no device) hand the unit back with why and leave the
/// plugin inactive.
#[test]
fn reactivate_rearms_the_unit_at_new_terms_and_a_refusal_leaves_the_plugin_inactive() {
    use super::clap_engine::Clap;
    use super::engine_slot::OwnedPlugin;
    use crate::engine_io::unit_host::Terms;
    use lf_engine::UnitProcessor;

    let mut instance = fixture_instance();
    let obs = unsafe { state(instance.raw_instance() as *const clap_plugin) }.obs.clone();
    let faults = Arc::new(AtomicU32::new(0));
    let unit = bare_unit(&mut instance, &faults);
    // SAFETY: as `fixture_instance`: the static entry outlives every instance.
    let entry = unsafe { PluginEntry::load_from_raw(&ENTRY, c"bleeploop-restart-fixture.clap") }.unwrap();
    let mut plugin = Clap::bare(instance, entry);

    let terms = Terms { generation: 7, rate: 44_100, max_block: 512 };
    let unit = plugin.reactivate(unit, terms).unwrap_or_else(|(_, e)| panic!("reactivate failed: {e}"));
    assert_eq!((obs.deactivations.load(Relaxed), obs.activations.load(Relaxed)), (1, 2), "deactivated, then activated");
    assert_eq!(f64::from_bits(obs.activated_rate.load(Relaxed)), 44_100.0);
    assert_eq!(obs.activated_max_frames.load(Relaxed), 512);
    let unit = std::thread::spawn(move || {
        let mut unit = unit;
        let (input, mut out) = ([0.0f32; 512], [0.0f32; 512]);
        unit.process_mono(0, &input, &[], &mut out);
        unit.stop();
        unit
    })
    .join()
    .unwrap();
    assert_eq!(obs.processes.load(Relaxed), 1, "a 512-frame block in one call: re-armed for the new max");
    assert_eq!(faults.load(Relaxed), 0);

    let refused = Terms { generation: 8, rate: 0, max_block: 512 };
    let (back, why) = match plugin.reactivate(unit, refused) {
        Ok(_) => panic!("rate 0 must be refused"),
        Err(refusal) => refusal,
    };
    assert_eq!(why, "no audio device is open");
    assert!(!obs.active.load(Relaxed), "the plugin is left inactive");
    assert_eq!((obs.deactivations.load(Relaxed), obs.activations.load(Relaxed)), (2, 2));
    plugin.release(Some(back));
    assert!(!obs.contract_violation.load(Relaxed));
}

/// CLAP discards the output of a process call that returns an error: the unit plays nothing from
/// that slice, though the plugin wrote it, and latches the fault.
#[test]
fn a_failed_process_call_plays_silence_and_latches_the_fault() {
    use super::engine_slot::FAULT_PROCESS;
    #[allow(unused_imports)]
    use crate::host::clap::ProcessMono;
    use lf_engine::UnitProcessor;

    let mut instance = fixture_instance();
    let s = unsafe { state(instance.raw_instance() as *const clap_plugin) };
    s.output_level.store(0.5f32.to_bits(), Relaxed);
    s.fail_process.store(true, Relaxed);
    let faults = Arc::new(AtomicU32::new(0));
    let unit = bare_unit(&mut instance, &faults);
    let obs = s.obs.clone();
    let mut unit = std::thread::spawn(move || {
        let mut unit = unit;
        let (input, mut out) = ([0.0f32; 64], [1.0f32; 64]);
        unit.process_mono(0, &input, &[], &mut out);
        assert!(out.iter().all(|&x| x == 0.0), "the failed call's output is discarded");
        obs.fail_process.store(false, Relaxed);
        unit.process_mono(64, &input, &[], &mut out);
        assert!(out.iter().all(|&x| x == 0.5), "the next call that succeeds plays");
        unit.stop();
        unit
    })
    .join()
    .unwrap();
    assert_eq!(s.processes.load(Relaxed), 2);
    assert_eq!(faults.load(Relaxed), FAULT_PROCESS, "the failure latched");
    instance.deactivate(unit.take_stopped().unwrap());
    assert!(!s.contract_violation.load(Relaxed));
}

/// The unit on its own: a stereo CLAP effect driven directly through `UnitProcessor::process` keeps
/// distinct left and right impulses on their own sides across slices, with nothing of one side on
/// the other.
#[test]
fn a_stereo_unit_keeps_left_and_right_apart() {
    use lf_engine::{grid::Meter, UnitCx, UnitProcessor};

    let mut instance = fixture_instance();
    let s = unsafe { state(instance.raw_instance() as *const clap_plugin) };
    s.inputs.store(2, Relaxed);
    let faults = Arc::new(AtomicU32::new(0));
    let unit = bare_unit(&mut instance, &faults);
    let mut unit = std::thread::spawn(move || {
        let mut unit = unit;
        let (mut in_l, mut in_r) = ([0.0f32; 160], [0.0f32; 160]);
        in_l[5] = 1.0;
        in_l[70] = -0.75;
        in_r[19] = 0.5;
        in_r[135] = 0.25;
        let (mut out_l, mut out_r) = ([1.0f32; 160], [1.0f32; 160]);
        let cx = UnitCx { frame: 0, dsp_frame: 0, bpm: 120.0, meter: Meter::FOUR_FOUR };
        unit.process(&cx, [&in_l, &in_r], &[], [&mut out_l, &mut out_r]);
        assert_eq!(out_l, in_l, "left carries only the left impulses, across every slice");
        assert_eq!(out_r, in_r, "right carries only the right impulses, across every slice");
        unit.stop();
        unit
    })
    .join()
    .unwrap();
    assert_eq!(s.processes.load(Relaxed), 3, "160 frames at 64 max: 64 + 64 + 32");
    assert_eq!(faults.load(Relaxed), 0);
    instance.deactivate(unit.take_stopped().unwrap());
    assert!(!s.contract_violation.load(Relaxed));
}

/// The unit on its own: a mono CLAP effect (`1` in, `1` out via `clap.audio-ports`) receives the
/// stereo pair's mean and copies its one output channel to both sides across slices
/// (`docs/unit-contract.md` §7).
#[test]
fn a_mono_unit_folds_input_to_the_mean_and_copies_output_to_both_sides() {
    use lf_engine::{grid::Meter, UnitCx, UnitProcessor};

    let mut instance = fixture_instance();
    let s = unsafe { state(instance.raw_instance() as *const clap_plugin) };
    s.inputs.store(1, Relaxed);
    s.outputs.store(1, Relaxed);
    let faults = Arc::new(AtomicU32::new(0));
    let unit = bare_unit(&mut instance, &faults);
    let mut unit = std::thread::spawn(move || {
        let mut unit = unit;
        let (mut in_l, mut in_r) = ([0.0f32; 160], [0.0f32; 160]);
        in_l[5] = 1.0;
        in_r[19] = 0.5;
        in_l[70] = 0.75;
        in_r[70] = -0.25;
        in_l[135] = -0.5;
        in_r[135] = 0.5;
        in_l[140] = 0.75;
        in_r[140] = 0.25;
        let mut expected = [0.0f32; 160];
        expected[5] = 0.5;
        expected[19] = 0.25;
        expected[70] = 0.25;
        expected[140] = 0.5;
        let (mut out_l, mut out_r) = ([1.0f32; 160], [1.0f32; 160]);
        let cx = UnitCx { frame: 0, dsp_frame: 0, bpm: 120.0, meter: Meter::FOUR_FOUR };
        unit.process(&cx, [&in_l, &in_r], &[], [&mut out_l, &mut out_r]);
        assert_eq!(out_l, expected, "left carries the mono output of the pair's mean");
        assert_eq!(out_r, expected, "right carries the same mono output");
        unit.stop();
        unit
    })
    .join()
    .unwrap();
    assert_eq!(s.processes.load(Relaxed), 3, "160 frames at 64 max: 64 + 64 + 32");
    assert_eq!(faults.load(Relaxed), 0);
    instance.deactivate(unit.take_stopped().unwrap());
    assert!(!s.contract_violation.load(Relaxed));
}

// ── chain unit owner (`unit_owner::serve_unit`) over the same CLAP fixture ──────────────────────

mod unit_owner {
    use super::super::engine_slot::{self, PluginFormat};
    use super::super::unit_owner::{self, Answer, UnitCtx};
    use super::super::super::tone::{self, TempDir, ToneIdentity};
    use super::super::super::state::ToneRestore;
    use super::*;
    use crate::engine_io::chain::{ChainState, PlateState, UnitState};
    use crate::engine_io::test_rig::TestDevice;
    use crate::engine_io::unit_host::{Activated, Back, OpState, Removal, Spec};
    use crate::engine_io::EngineHost;
    use lf_engine::grid::Frame;
    use lf_engine::{
        Command, Event, LaneState, SeatOp, SeatOutcome, SlotEvent, SlotKind, TimedCommand, UnitCx, UnitId, UnitInfo,
        UnitKind, UnitProcessor, SLOT_COUNT,
    };

    fn device(rate: u32) -> TestDevice {
        TestDevice::start(rate, 256, Duration::from_millis(5), |_| 0.1)
    }

    fn identity() -> ToneIdentity {
        ToneIdentity {
            format: "clap".to_string(),
            path: "bleeploop-restart-fixture.clap".to_string(),
            id: "bleeploop.restart-fixture".to_string(),
        }
    }

    fn installed(device: &TestDevice, slot: usize) -> Option<(SlotKind, Frame)> {
        device.host().core.rt.lock().unwrap().engine.as_ref().unwrap().slot(slot)
    }

    fn request_restart(obs: &Observed) {
        let host = unsafe { *obs.host.load(Relaxed) };
        std::thread::spawn(move || unsafe { (host.request_restart.unwrap())(&host) })
            .join()
            .unwrap();
    }

    fn start_take(device: &TestDevice) {
        let host = device.host();
        host.send(TimedCommand { frame: None, command: Command::SetBpm(240.0) }).unwrap();
        host.send(TimedCommand { frame: None, command: Command::SetFixedLength(false) }).unwrap();
        host.send(TimedCommand { frame: None, command: Command::RecDub(0) }).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut ev = Vec::new();
            let (_, ov) = host.drain_feed(&mut ev);
            if let Some(ov) = ov {
                let l = ov.lane(0);
                if l.state == LaneState::Recording && l.frames > 0 {
                    break;
                }
            }
            assert!(Instant::now() < deadline, "recording take never started");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn stop_take(device: &TestDevice) {
        let host = device.host();
        host.send(TimedCommand { frame: None, command: Command::Stop(0) }).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut ev = Vec::new();
            let (_, ov) = host.drain_feed(&mut ev);
            if let Some(ov) = ov {
                if ov.lane(0).state != LaneState::Recording {
                    break;
                }
            }
            assert!(Instant::now() < deadline, "recording take never stopped");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Tiny pass-through unit with a fixed latency, submitted straight through `UnitHost` in tests.
    struct FixedLatencyUnit {
        latency: Frame,
    }

    impl UnitProcessor for FixedLatencyUnit {
        fn info(&self) -> UnitInfo {
            UnitInfo { kind: UnitKind::Plugin, source: false, latency: self.latency }
        }

        fn process(&mut self, _cx: &UnitCx, input: [&[f32]; 2], _events: &[SlotEvent], out: [&mut [f32]; 2]) {
            out[0].copy_from_slice(input[0]);
            out[1].copy_from_slice(input[1]);
        }

        fn stop(&mut self) {}

        fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
            self
        }
    }

    fn rendered_above(device: &TestDevice, level: f32) -> bool {
        device.output.lock().unwrap().iter().rev().take(256).all(|&x| x > level)
    }

    /// Feed stand-in: drains engine events every ~2 ms so `UnitHost::poll` and `chains` advance.
    struct FeedStandIn {
        stop: Arc<AtomicBool>,
        chains: Arc<Mutex<Arc<[ChainState; SLOT_COUNT]>>>,
        events: Arc<Mutex<Vec<Event>>>,
        overview: Arc<Mutex<Option<Arc<lf_engine::Overview>>>>,
        join: Option<std::thread::JoinHandle<()>>,
    }

    impl FeedStandIn {
        fn start(host: EngineHost) -> Self {
            Self::start_with_blocks(host, None)
        }

        fn start_synced(host: EngineHost, blocks: Arc<AtomicU64>) -> Self {
            Self::start_with_blocks(host, Some(blocks))
        }

        fn start_with_blocks(host: EngineHost, blocks: Option<Arc<AtomicU64>>) -> Self {
            let stop = Arc::new(AtomicBool::new(false));
            let mut first_ev = Vec::new();
            let first = host.drain_feed_taps(&mut first_ev, None, None);
            let chains = Arc::new(Mutex::new(first.chains.chains));
            let events = Arc::new(Mutex::new(first_ev));
            let overview = Arc::new(Mutex::new(first.overview));
            let (stop_t, chains_t, events_t, overview_t) =
                (stop.clone(), chains.clone(), events.clone(), overview.clone());
            let join = std::thread::Builder::new()
                .name("lf-test-feed".to_string())
                .spawn(move || {
                    let mut ev = Vec::new();
                    let mut last_block = blocks.as_ref().map_or(0, |b| b.load(Acquire));
                    while !stop_t.load(Acquire) {
                        if let Some(b) = blocks.as_ref() {
                            let cur = b.load(Acquire);
                            if cur == last_block {
                                std::thread::sleep(Duration::from_millis(1));
                                continue;
                            }
                            last_block = cur;
                            // `TestDevice` just entered its per-block sleep: peek `Event::Seat`
                            // before `drain_feed_taps` projects and strips it.
                            if let Ok(mut ends) = host.core.ends.lock() {
                                if let Some(ends) = ends.as_mut() {
                                    let slots = ends.events.slots();
                                    if slots > 0 {
                                        if let Ok(chunk) = ends.events.read_chunk(slots) {
                                            let (a, b) = chunk.as_slices();
                                            let mut log = events_t.lock().unwrap();
                                            for &e in a.iter().chain(b.iter()) {
                                                if matches!(e, Event::Seat { .. }) {
                                                    log.push(e);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        ev.clear();
                        let drain = host.drain_feed_taps(&mut ev, None, None);
                        *chains_t.lock().unwrap() = drain.chains.chains;
                        if drain.overview.is_some() {
                            *overview_t.lock().unwrap() = drain.overview;
                        }
                        if !ev.is_empty() {
                            events_t.lock().unwrap().extend_from_slice(&ev);
                        }
                        if blocks.is_none() {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                    }
                })
                .expect("spawn feed stand-in");
            FeedStandIn { stop, chains, events, overview, join: Some(join) }
        }

        fn plate(&self, slot: usize, id: UnitId) -> Option<UnitState> {
            self.chains.lock().unwrap()[slot].units.iter().find(|u| u.id == id).cloned()
        }

        fn chain(&self, slot: usize) -> ChainState {
            self.chains.lock().unwrap()[slot].clone()
        }

        fn lane(&self, lane: usize) -> Option<lf_engine::LaneView> {
            self.overview.lock().unwrap().as_ref().map(|ov| ov.lane(lane))
        }

        fn events(&self) -> Vec<Event> {
            self.events.lock().unwrap().clone()
        }
    }

    impl Drop for FeedStandIn {
        fn drop(&mut self) {
            self.stop.store(true, Release);
            if let Some(join) = self.join.take() {
                let _ = join.join();
            }
        }
    }

    fn unit_runner(
        obs: Vec<Arc<Observed>>,
        plugin_id: &'static str,
        ended: Option<Arc<AtomicBool>>,
    ) -> impl FnOnce(UnitCtx) -> Result<(), String> + Send + 'static {
        move |ctx| {
            let res = super::super::clap_engine::run_unit(ctx, plugin_id, move || {
                OBSERVE.with(|o| *o.borrow_mut() = obs);
                // SAFETY: static fixture entry whose callbacks obey the CLAP ABI.
                unsafe { PluginEntry::load_from_raw(&ENTRY, c"bleeploop-restart-fixture.clap") }
                    .map_err(|e| e.to_string())
            });
            if let Some(flag) = ended {
                flag.store(true, Release);
            }
            res
        }
    }

    #[test]
    fn a_unit_owner_seats_serves_and_on_removed_saves_retires_and_tears_down() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let feed = FeedStandIn::start(device.host().clone());
        let dir = TempDir::new("unit-owner-remove");
        let ident = identity();
        let unit_id: UnitId = 7;

        let obs = Arc::new(Observed::default());
        obs.output_level.store(0.25f32.to_bits(), Relaxed);
        obs.param_count.store(3, Relaxed);

        let unit_host = device
            .host()
            .unit(0, unit_id, Spec::Plugin(ident.clone()), 0, false)
            .expect("reserve chain unit");
        let (handle, answer) = unit_owner::spawn_unit(
            PluginFormat::Clap,
            unit_host,
            0,
            Arc::new(|_| {}),
            Some(dir.unit_binding(unit_id, 0, ident.clone())),
            unit_runner(vec![obs.clone()], "bleeploop.restart-fixture", None),
        )
        .expect("unit owner seats");

        assert_eq!(answer, Answer::Seated { name: "Restart fixture".to_string(), tone: None });
        assert_eq!(handle.name(), "Restart fixture");
        assert!(
            wait_for(|| feed
                .plate(0, unit_id)
                .is_some_and(|p| p.state == PlateState::Seated && p.name == "Restart fixture")),
            "plate reads seated with the fixture's name"
        );
        assert!(wait_for(|| obs.processes.load(Relaxed) > 8), "the engine processes the unit");
        assert!(wait_for(|| rendered_above(&device, 0.2)), "its output reaches the device");

        let params = handle.list_params().expect("list_params answers");
        assert_eq!(params.iter().map(|p| p.id).collect::<Vec<_>>(), vec![100, 101, 102]);

        // Change state and save explicitly: lands in `unit-<id>.tone`.
        *obs.saved.lock().unwrap() = b"unit-tone-v1".to_vec();
        let taken = tone::decode(&handle.take_tone().expect("take_tone")).expect("valid tone bytes");
        assert_eq!(taken.state, b"unit-tone-v1");
        let unit_file = dir.0.join(ToneIdentity::unit_file_name(unit_id));
        assert!(unit_file.exists(), "unit-<id>.tone exists after take_tone");
        let on_disk = tone::decode(&std::fs::read(&unit_file).unwrap()).unwrap();
        assert_eq!(on_disk.state, b"unit-tone-v1");

        // Mark dirty with a newer state right before removal so Removed saves AND retires it.
        *obs.saved.lock().unwrap() = b"unit-tone-v2".to_vec();
        obs.dirty_on_callback.store(true, Relaxed);
        let host_ptr = obs.host.load(Relaxed);
        assert!(!host_ptr.is_null());
        // SAFETY: live fixture clap_host pointer; request_callback is thread-safe.
        unsafe { ((*host_ptr).request_callback.unwrap())(host_ptr) };

        let Removal::Sent(rm_op) = device.host().remove_unit(unit_id).expect("remove_unit") else {
            panic!("seated unit removal must be Sent");
        };
        assert_eq!(device.host().wait_op(rm_op, PATIENCE).expect("wait_op"), OpState::Applied);

        assert!(wait_for(|| handle.is_finished()), "owner ends by itself after Back::Removed");
        handle.stop().expect("owner ended cleanly");

        assert_eq!((obs.starts.load(Relaxed), obs.stops.load(Relaxed)), (1, 1));
        assert_eq!(obs.deactivations.load(Relaxed), 1);
        let (stop, deactivate, destroy) =
            (obs.stop_seq.load(Relaxed), obs.deactivate_seq.load(Relaxed), obs.destroy_seq.load(Relaxed));
        assert!(
            0 < stop && stop < deactivate && deactivate < destroy,
            "stop {stop} < deactivate {deactivate} < destroy {destroy}"
        );
        assert!(!unit_file.exists(), "unit file is gone after retire");
        let retired = dir.store().load(0, &ident).expect("load retired slot tone").expect("slot tone exists");
        assert_eq!(retired.state, b"unit-tone-v2", "slot file holds the saved state after retire");
        assert!(!obs.contract_violation.load(Relaxed), "no contract violation");
        assert_eq!(obs.rt_calls_off_device.load(Relaxed), 0, "no RT call off the device thread");
    }

    #[test]
    fn an_install_the_engine_refuses_fails_its_plate_and_releases_the_plugin() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let feed = FeedStandIn::start(device.host().clone());
        let ident = identity();

        // Part 1: engine refuses an install whose latency exceeds max_chain_latency.
        let obs = Arc::new(Observed::default());
        obs.latency.store(lf_engine::max_chain_latency(48_000) as u32 + 1, Relaxed);
        let ended = Arc::new(AtomicBool::new(false));
        let unit_host = device.host().unit(0, 1, Spec::Plugin(ident.clone()), 0, false).unwrap();
        let err = match unit_owner::spawn_unit(
            PluginFormat::Clap,
            unit_host,
            0,
            Arc::new(|_| {}),
            None,
            unit_runner(vec![obs.clone()], "bleeploop.restart-fixture", Some(ended.clone())),
        ) {
            Err(e) => e,
            Ok((_, ans)) => panic!("expected latency refusal, got {ans:?}"),
        };
        assert!(err.contains("Latency"), "error should name the refusal: {err}");
        assert!(ended.load(Acquire), "owner thread has ended");
        assert!(
            wait_for(|| feed.plate(0, 1).is_some_and(|p| p.state == PlateState::Failed && !p.parked)),
            "plate reads failed"
        );
        assert_eq!(obs.activations.load(Relaxed), 1, "activated once");
        assert_eq!(obs.deactivations.load(Relaxed), 1, "deactivated once");
        assert!(obs.destroy_seq.load(Relaxed) > 0, "destroyed once");
        assert_eq!(obs.starts.load(Relaxed), obs.stops.load(Relaxed), "started as often as stopped");
        assert!(!obs.contract_violation.load(Relaxed), "no contract violation");

        // Part 2: load error (an id the entry does not hold).
        let obs2 = Arc::new(Observed::default());
        let ended2 = Arc::new(AtomicBool::new(false));
        let unit_host2 = device.host().unit(0, 2, Spec::Plugin(ident), 0, false).unwrap();
        let err2 = match unit_owner::spawn_unit(
            PluginFormat::Clap,
            unit_host2,
            0,
            Arc::new(|_| {}),
            None,
            unit_runner(vec![obs2.clone()], "bleeploop.no-such-plugin", Some(ended2.clone())),
        ) {
            Err(e) => e,
            Ok((_, ans)) => panic!("expected load error, got {ans:?}"),
        };
        assert!(!err2.is_empty(), "load error message");
        assert!(ended2.load(Acquire), "owner thread has ended after load error");
        assert!(
            wait_for(|| feed.plate(0, 2).is_some_and(|p| p.state == PlateState::Failed && !p.parked)),
            "plate reads failed after load error"
        );
        assert_eq!(obs2.activations.load(Relaxed), 0, "nothing activated on load error");
    }

    #[test]
    fn a_restart_that_changes_latency_mid_take_reseats_after_the_take() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let feed = FeedStandIn::start_synced(device.host().clone(), device.blocks.clone());
        let ident = identity();
        let unit_id: UnitId = 1;

        let obs = Arc::new(Observed::default());
        obs.inputs.store(2, Relaxed);
        obs.latency.store(0, Relaxed);
        obs.param_count.store(2, Relaxed);

        let unit_host = device.host().unit(0, unit_id, Spec::Plugin(ident), 0, false).unwrap();
        let (handle, answer) = unit_owner::spawn_unit(
            PluginFormat::Clap,
            unit_host,
            0,
            Arc::new(|_| {}),
            None,
            unit_runner(vec![obs.clone()], "bleeploop.restart-fixture", None),
        )
        .expect("unit seats");
        assert_eq!(answer, Answer::Seated { name: "Restart fixture".to_string(), tone: None });
        assert!(wait_for(|| obs.processes.load(Relaxed) > 8));
        assert_eq!(installed(&device, 0), Some((SlotKind::Effect, 0)));

        // Drive the take without calling `drain_feed` directly so `FeedStandIn` captures every event.
        let host = device.host();
        host.send(TimedCommand { frame: None, command: Command::SetBpm(240.0) }).unwrap();
        host.send(TimedCommand { frame: None, command: Command::SetFixedLength(false) }).unwrap();
        host.send(TimedCommand { frame: None, command: Command::RecDub(0) }).unwrap();
        assert!(
            wait_for(|| feed.lane(0).is_some_and(|l| l.state == LaneState::Recording && l.frames > 0)),
            "recording take started"
        );

        obs.latency.store(128, Relaxed);
        request_restart(&obs);

        assert!(
            wait_for(|| obs.deactivations.load(Relaxed) == 1
                && obs.activations.load(Relaxed) == 2
                && feed.chain(0).held),
            "withdrawal applies at once and reseat is held while the take lasts"
        );
        assert!(
            feed.lane(0).is_some_and(|l| l.state == LaneState::Recording),
            "the take is still recording"
        );
        assert_eq!(obs.stops.load(Relaxed), 1);
        assert_eq!(obs.starts.load(Relaxed), 1);

        let params = handle.list_params().expect("list_params answers during the hold");
        assert_eq!(params.iter().map(|p| p.id).collect::<Vec<_>>(), vec![100, 101]);

        for _ in 0..10 {
            assert!(device.wait_blocks(1));
            assert_eq!(
                installed(&device, 0),
                Some((SlotKind::Effect, 0)),
                "slot engine latency stays 0 on every read while the take lasts"
            );
        }

        let processed_before = obs.processes.load(Relaxed);
        // Commit the take on the grid (`RecDub` closes a recording take into `Playing`).
        host.send(TimedCommand { frame: None, command: Command::RecDub(0) }).unwrap();
        assert!(
            wait_for(|| feed.lane(0).is_some_and(|l| l.state == LaneState::Playing && l.frames > 0)),
            "recording take committed into a playing loop"
        );

        assert!(
            wait_for(|| feed.plate(0, unit_id).is_some_and(|p| p.state == PlateState::Seated && p.latency == 128 && !p.held)
                && !feed.chain(0).held
                && installed(&device, 0) == Some((SlotKind::Effect, 128))
                && obs.starts.load(Relaxed) == 2
                && obs.processes.load(Relaxed) > processed_before + 8),
            "after the take ends the unit is seated at 128 and processing again"
        );

        // Verify the take's loop length stayed on the grid (240 BPM @ 48 kHz = 12_000 frames/beat)
        // and the held reseat landed on the first 256-frame block after the take ended.
        let events = feed.events();
        let (commit_frame, loop_frames) = events
            .iter()
            .find_map(|e| match *e {
                Event::Lane { frame, lane: 0, info } if info.state == LaneState::Playing && info.length > 0 => {
                    Some((frame, info.length))
                }
                _ => None,
            })
            .expect("captured the take's commit Event::Lane");
        let beat_frames: Frame = 48_000 * 60 / 240;
        assert_eq!(
            loop_frames % beat_frames,
            0,
            "committed loop length {loop_frames} must stay on the {beat_frames}-frame beat grid"
        );
        let master = events
            .iter()
            .rev()
            .find_map(|e| match *e {
                Event::Transport { master, .. } if master > 0 => Some(master),
                _ => None,
            })
            .expect("captured committed master loop length");
        assert_eq!(loop_frames, master, "lane loop length matches the master grid");

        let wait_idx = events
            .iter()
            .position(|e| {
                matches!(
                    *e,
                    Event::Seat { slot: 0, unit, op: SeatOp::Install, outcome: SeatOutcome::Waiting, .. }
                        if unit == unit_id
                )
            })
            .expect("captured the held reseat's Waiting Event::Seat");
        let reseat_frame = events[wait_idx + 1..]
            .iter()
            .find_map(|e| match *e {
                Event::Seat { frame, slot: 0, unit, op: SeatOp::Install, outcome: SeatOutcome::Applied }
                    if unit == unit_id =>
                {
                    Some(frame)
                }
                _ => None,
            })
            .expect("captured the held reseat's Applied Event::Seat");
        let first_block_after = ((commit_frame + 255) / 256) * 256;
        assert_eq!(
            reseat_frame, first_block_after,
            "held reseat must land on the first block after the take ends (commit={commit_frame}, expected={first_block_after}, got={reseat_frame})"
        );

        let Removal::Sent(rm_op) = device.host().remove_unit(unit_id).expect("remove_unit") else {
            panic!("seated unit removal must be Sent");
        };
        assert_eq!(device.host().wait_op(rm_op, PATIENCE).expect("wait_op"), OpState::Applied);
        assert!(wait_for(|| handle.is_finished()));
        handle.stop().expect("owner ended cleanly");
        assert!(!obs.contract_violation.load(Relaxed), "no contract violation");
        assert_eq!(obs.rt_calls_off_device.load(Relaxed), 0, "no RT call off the device thread");
    }

    #[test]
    fn a_withdraw_queued_behind_another_units_held_install_waits_it_out() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let feed = FeedStandIn::start(device.host().clone());
        let ident_a = identity();
        let ident_b = ToneIdentity {
            format: "clap".to_string(),
            path: "dummy-b.clap".to_string(),
            id: "bleeploop.dummy-b".to_string(),
        };
        let unit_a: UnitId = 1;
        let unit_b: UnitId = 2;

        let obs = Arc::new(Observed::default());
        obs.inputs.store(2, Relaxed);
        obs.latency.store(0, Relaxed);
        obs.param_count.store(2, Relaxed);

        let a_host = device.host().unit(0, unit_a, Spec::Plugin(ident_a), 0, false).unwrap();
        let (handle, answer) = unit_owner::spawn_unit(
            PluginFormat::Clap,
            a_host,
            0,
            Arc::new(|_| {}),
            None,
            unit_runner(vec![obs.clone()], "bleeploop.restart-fixture", None),
        )
        .expect("unit A seats");
        assert_eq!(answer, Answer::Seated { name: "Restart fixture".to_string(), tone: None });
        assert!(wait_for(|| obs.processes.load(Relaxed) > 8));

        start_take(&device);

        // Submit unit B (latency 64) straight through its UnitHost: held by the active take.
        let b_host = device.host().unit(0, unit_b, Spec::Plugin(ident_b), 1, false).unwrap();
        b_host.activated(Activated { name: "Held B".to_string(), source: false, latency: 64 });
        let b_terms = b_host.terms().expect("engine open");
        let b_op = b_host
            .submit(Box::new(FixedLatencyUnit { latency: 64 }), b_terms)
            .map_err(|(_, e)| e)
            .expect("submit B");
        assert!(wait_for(|| b_host.poll(b_op) == OpState::Held && feed.chain(0).held), "B is held");

        // Ask A's plugin to restart: its withdrawal queues behind B's held install.
        request_restart(&obs);

        let hold_until = Instant::now() + Duration::from_millis(1200);
        while Instant::now() < hold_until {
            let params = handle.list_params().expect("A keeps answering list_params during the hold");
            assert_eq!(params.iter().map(|p| p.id).collect::<Vec<_>>(), vec![100, 101]);
            assert_eq!(
                obs.activations.load(Relaxed),
                1,
                "A's withdrawal waits behind B's held install (still activated once)"
            );
            assert_eq!(obs.deactivations.load(Relaxed), 0, "A has not deactivated while B holds the port");
            assert!(!handle.is_finished(), "A's owner does not give up");
            assert_eq!(b_host.poll(b_op), OpState::Held, "B stays held while the take lasts");
            std::thread::sleep(Duration::from_millis(100));
        }

        let processed_before = obs.processes.load(Relaxed);
        stop_take(&device);

        assert!(wait_for(|| b_host.poll(b_op) == OpState::Applied), "B applies after stop_take");
        assert!(
            wait_for(|| obs.deactivations.load(Relaxed) == 1
                && obs.activations.load(Relaxed) == 2
                && obs.starts.load(Relaxed) == 2
                && obs.processes.load(Relaxed) > processed_before + 8
                && feed.plate(0, unit_a).is_some_and(|p| p.state == PlateState::Seated && !p.held)),
            "A's withdrawal and reseat apply after B, and A processes again"
        );

        let Removal::Sent(b_rm) = device.host().remove_unit(unit_b).expect("remove B") else {
            panic!("seated unit B removal must be Sent");
        };
        assert_eq!(device.host().wait_op(b_rm, PATIENCE).expect("wait_op B"), OpState::Applied);
        assert!(
            wait_for(|| matches!(b_host.take_back(), Some(Back::Removed(_)))),
            "B's box comes back through take_back"
        );
        b_host.release();

        let Removal::Sent(a_rm) = device.host().remove_unit(unit_a).expect("remove A") else {
            panic!("seated unit A removal must be Sent");
        };
        assert_eq!(device.host().wait_op(a_rm, PATIENCE).expect("wait_op A"), OpState::Applied);
        assert!(wait_for(|| handle.is_finished()));
        handle.stop().expect("owner ended cleanly");
        assert!(!obs.contract_violation.load(Relaxed), "no contract violation");
        assert_eq!(obs.rt_calls_off_device.load(Relaxed), 0, "no RT call off the device thread");
    }

    #[test]
    fn an_eviction_reactivates_the_unit_at_the_new_terms_and_reseats_it() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let feed = FeedStandIn::start(device.host().clone());
        let ident = identity();
        let unit_id: UnitId = 3;

        let obs = Arc::new(Observed::default());
        obs.inputs.store(2, Relaxed);
        obs.latency.store(0, Relaxed);

        let unit_host = device.host().unit(0, unit_id, Spec::Plugin(ident), 0, false).unwrap();
        let (handle, answer) = unit_owner::spawn_unit(
            PluginFormat::Clap,
            unit_host,
            0,
            Arc::new(|_| {}),
            None,
            unit_runner(vec![obs.clone()], "bleeploop.restart-fixture", None),
        )
        .expect("unit seats");
        assert_eq!(answer, Answer::Seated { name: "Restart fixture".to_string(), tone: None });
        assert!(wait_for(|| obs.processes.load(Relaxed) > 8));

        let processed_before = obs.processes.load(Relaxed);
        device.rebuild_at(44_100, 512);

        assert!(
            wait_for(|| obs.activations.load(Relaxed) == 2
                && obs.deactivations.load(Relaxed) == 1
                && f64::from_bits(obs.activated_rate.load(Relaxed)) == 44_100.0
                && obs.activated_max_frames.load(Relaxed) == 512
                && obs.starts.load(Relaxed) == 2
                && obs.processes.load(Relaxed) > processed_before + 8
                && feed.plate(0, unit_id).is_some_and(|p| p.state == PlateState::Seated)),
            "evicted unit reactivates at 44.1 kHz / 512 frames, reseats, and processes"
        );
        assert_eq!(installed(&device, 0), Some((SlotKind::Effect, 0)));

        let Removal::Sent(rm_op) = device.host().remove_unit(unit_id).expect("remove_unit") else {
            panic!("seated unit removal must be Sent");
        };
        assert_eq!(device.host().wait_op(rm_op, PATIENCE).expect("wait_op"), OpState::Applied);
        assert!(wait_for(|| handle.is_finished()));
        handle.stop().expect("owner ended cleanly");
        assert_eq!(obs.deactivations.load(Relaxed), 2, "deactivated by reactivate and by release");
        assert!(!obs.contract_violation.load(Relaxed), "no contract violation");
    }

    #[test]
    fn a_refused_reseat_parks_and_a_removal_dismisses_it() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let feed = FeedStandIn::start(device.host().clone());
        let dir = TempDir::new("unit-owner-parked-dismiss");
        let ident = identity();
        let unit_id: UnitId = 9;

        let obs = Arc::new(Observed::default());
        obs.inputs.store(2, Relaxed);
        obs.latency.store(0, Relaxed);
        obs.param_count.store(2, Relaxed);

        let unit_host = device.host().unit(0, unit_id, Spec::Plugin(ident.clone()), 0, false).unwrap();
        let (handle, answer) = unit_owner::spawn_unit(
            PluginFormat::Clap,
            unit_host,
            0,
            Arc::new(|_| {}),
            Some(dir.unit_binding(unit_id, 0, ident.clone())),
            unit_runner(vec![obs.clone()], "bleeploop.restart-fixture", None),
        )
        .expect("unit seats");
        assert_eq!(answer, Answer::Seated { name: "Restart fixture".to_string(), tone: None });
        assert!(wait_for(|| obs.processes.load(Relaxed) > 8));

        // Save a tone into `unit-<id>.tone` so we verify Dismissed retires it into the slot file.
        *obs.saved.lock().unwrap() = b"parked-tone-v1".to_vec();
        let _ = handle.take_tone().expect("take_tone");
        let unit_file = dir.0.join(ToneIdentity::unit_file_name(unit_id));
        assert!(unit_file.exists(), "unit-<id>.tone exists before restart");

        obs.latency.store(lf_engine::max_chain_latency(48_000) as u32 + 1, Relaxed);
        request_restart(&obs);

        assert!(
            wait_for(|| feed.plate(0, unit_id).is_some_and(|p| p.state == PlateState::Failed && p.parked)),
            "refused reseat parks the unit: plate reads failed and parked"
        );
        let params = handle.list_params().expect("list_params still answers while parked");
        assert_eq!(params.iter().map(|p| p.id).collect::<Vec<_>>(), vec![100, 101]);

        let Removal::Sent(rm_op) = device.host().remove_unit(unit_id).expect("remove_unit") else {
            panic!("parked unit removal must be Sent");
        };
        assert_eq!(device.host().wait_op(rm_op, PATIENCE).expect("wait_op"), OpState::Applied);

        assert!(wait_for(|| handle.is_finished()), "owner gets Dismissed and ends");
        handle.stop().expect("owner ended cleanly");

        assert_eq!(obs.deactivations.load(Relaxed), 2, "deactivated twice: reactivate and release");
        assert!(obs.destroy_seq.load(Relaxed) > 0, "destroyed once");
        assert!(wait_for(|| feed.plate(0, unit_id).is_none()), "plate is gone");
        assert!(!unit_file.exists(), "unit tone file is gone after retire");
        let retired = dir.store().load(0, &ident).expect("load retired slot tone").expect("slot tone exists");
        assert_eq!(retired.state, b"parked-tone-v1", "unit tone retired into the slot file");
        assert!(!obs.contract_violation.load(Relaxed), "no contract violation");
        assert_eq!(obs.rt_calls_off_device.load(Relaxed), 0, "no RT call off the device thread");
    }

    /// A second entry over the same factory whose `init`/`deinit` count into statics of its own: only
    /// the two-instance test loads it (other tests load `ENTRY` concurrently). clack caches an entry
    /// by its address, so one `init` covers every live instance and `deinit` follows the last release.
    static COUNTED_INITS: AtomicUsize = AtomicUsize::new(0);
    static COUNTED_DEINITS: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn counted_init(_: *const c_char) -> bool {
        COUNTED_INITS.fetch_add(1, Relaxed);
        true
    }
    unsafe extern "C" fn counted_deinit() {
        COUNTED_DEINITS.fetch_add(1, Relaxed);
    }
    static COUNTED_ENTRY: clap_plugin_entry = clap_plugin_entry {
        clap_version: CLAP_VERSION,
        init: Some(counted_init),
        deinit: Some(counted_deinit),
        get_factory: Some(factory),
    };

    /// `unit_runner` over `entry` instead of `ENTRY`.
    fn entry_runner(
        entry: &'static clap_plugin_entry,
        obs: Vec<Arc<Observed>>,
    ) -> impl FnOnce(UnitCtx) -> Result<(), String> + Send + 'static {
        move |ctx| {
            super::super::clap_engine::run_unit(ctx, "bleeploop.restart-fixture", move || {
                OBSERVE.with(|o| *o.borrow_mut() = obs);
                // SAFETY: static fixture entry whose callbacks obey the CLAP ABI.
                unsafe { PluginEntry::load_from_raw(entry, c"bleeploop-restart-fixture.clap") }.map_err(|e| e.to_string())
            })
        }
    }

    /// Seat a fixture unit `id` in `slot` through its own owner, with `tone` as its binding.
    fn seat(
        device: &TestDevice,
        slot: usize,
        id: UnitId,
        obs: &Arc<Observed>,
        tone: Option<tone::ToneBinding>,
    ) -> (unit_owner::UnitOwner, Answer) {
        let unit_host = device.host().unit(slot, id, Spec::Plugin(identity()), 0, false).expect("reserve chain unit");
        unit_owner::spawn_unit(
            PluginFormat::Clap,
            unit_host,
            0,
            Arc::new(|_| {}),
            tone,
            unit_runner(vec![obs.clone()], "bleeploop.restart-fixture", None),
        )
        .expect("unit owner answers")
    }

    /// Remove seated unit `id` and wait for its owner to end by itself; the owner's result.
    fn remove(device: &TestDevice, id: UnitId, handle: unit_owner::UnitOwner) -> Result<(), String> {
        let Removal::Sent(op) = device.host().remove_unit(id).expect("remove_unit") else {
            panic!("a seated unit's removal is sent");
        };
        assert_eq!(device.host().wait_op(op, PATIENCE).expect("wait_op"), OpState::Applied);
        assert!(wait_for(|| handle.is_finished()), "the owner ends by itself after its removal");
        handle.stop()
    }

    /// Mark the fixture's state changed through the host's `clap.state` from its main thread.
    fn mark_dirty(obs: &Observed) {
        let before = obs.marked_dirty.load(Relaxed);
        obs.dirty_on_callback.store(true, Relaxed);
        let host = obs.host.load(Relaxed);
        // SAFETY: the host lives while the plugin is loaded; `request_callback` is thread-safe.
        unsafe { ((*host).request_callback.unwrap())(host) };
        assert!(wait_for(|| obs.marked_dirty.load(Relaxed) > before), "the plugin marked its state dirty");
    }

    fn released_in_order(obs: &Observed) -> bool {
        let (stop, deactivate, destroy) =
            (obs.stop_seq.load(Relaxed), obs.deactivate_seq.load(Relaxed), obs.destroy_seq.load(Relaxed));
        0 < stop && stop < deactivate && deactivate < destroy
    }

    /// The plan's test: a same-id reload is two rows under one id. The outgoing owner A gets
    /// `Back::Superseded` once the candidate B's install applies, releases its plugin once, and its
    /// late state misses the unit's file, which a session redemption counted past A's load; A does not
    /// retire, so the slot's file stands. B restores the redeemed tone, processes, and retires on removal.
    #[test]
    fn a_superseded_owner_releases_once_and_its_late_save_misses_the_unit_file() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let feed = FeedStandIn::start(device.host().clone());
        let dir = TempDir::new("unit-owner-supersede");
        let ident = identity();
        let unit_id: UnitId = 21;
        let fixture_tone = |state: &[u8]| tone::Tone {
            identity: ident.clone(),
            name: "Restart fixture".to_string(),
            state: state.to_vec(),
        };
        dir.store().save_encoded(0, &tone::encode(&fixture_tone(b"slot-v0"))).unwrap();
        let slot_tone = || dir.store().load(0, &ident).unwrap().map(|t| t.state);
        let unit_file = dir.0.join(ToneIdentity::unit_file_name(unit_id));
        let unit_tone = || std::fs::read(&unit_file).ok().map(|b| tone::decode(&b).unwrap().state);

        let a = Arc::new(Observed::default());
        a.output_level.store(0.25f32.to_bits(), Relaxed);
        let (owner_a, answer) = seat(&device, 0, unit_id, &a, Some(dir.unit_binding(unit_id, 0, ident.clone())));
        assert_eq!(answer, Answer::Seated { name: "Restart fixture".to_string(), tone: Some(ToneRestore::Restored) });
        assert_eq!(*a.saved.lock().unwrap(), b"slot-v0", "A starts from the slot's tone");
        assert!(wait_for(|| a.processes.load(Relaxed) > 8), "A processes");

        // The chain's token redemption: the unit's file, counted past A's load.
        let key = dir.unit_binding(unit_id, 0, ident.clone()).key;
        let imported = dir.store().redeem(&key, fixture_tone(b"redeemed")).expect("redeem");
        // A changes after it: a change A would save on its own (debounced) or at its release.
        *a.saved.lock().unwrap() = b"A-late".to_vec();
        mark_dirty(&a);

        let candidate = device.host().reload_unit(unit_id, ident.clone()).expect("reload candidate");
        let b = Arc::new(Observed::default());
        b.output_level.store(0.5f32.to_bits(), Relaxed);
        let mut binding = dir.unit_binding(unit_id, 0, ident.clone());
        binding.imported = Some(imported);
        let (owner_b, answer) = unit_owner::spawn_unit(
            PluginFormat::Clap,
            candidate,
            0,
            Arc::new(|_| {}),
            Some(binding),
            unit_runner(vec![b.clone()], "bleeploop.restart-fixture", None),
        )
        .expect("the candidate's owner answers");
        assert_eq!(answer, Answer::Seated { name: "Restart fixture".to_string(), tone: Some(ToneRestore::Restored) });
        assert_eq!(*b.saved.lock().unwrap(), b"redeemed", "B restores the redeemed tone");

        assert!(wait_for(|| owner_a.is_finished()), "A ends by itself on Back::Superseded");
        owner_a.stop().expect("A released its plugin in order");
        assert_eq!((a.starts.load(Relaxed), a.stops.load(Relaxed)), (1, 1), "A started and stopped once");
        assert_eq!(a.deactivations.load(Relaxed), 1, "A deactivated once");
        assert!(released_in_order(&a), "A: stop < deactivate < destroy");
        assert_eq!(unit_tone().as_deref(), Some(&b"redeemed"[..]), "A's late state missed the unit's file");
        assert_eq!(slot_tone().as_deref(), Some(&b"slot-v0"[..]), "A did not retire: the slot's file stands");

        let processed = b.processes.load(Relaxed);
        assert!(wait_for(|| b.processes.load(Relaxed) > processed + 8), "B processes");
        assert!(wait_for(|| rendered_above(&device, 0.4)), "B's output reaches the device");
        assert!(
            wait_for(|| feed.plate(0, unit_id).is_some_and(|p| p.state == PlateState::Seated && !p.reloading)),
            "one seated plate under the id"
        );

        remove(&device, unit_id, owner_b).expect("B released its plugin in order");
        assert!(released_in_order(&b), "B: stop < deactivate < destroy");
        assert!(!unit_file.exists(), "B retired the unit's file");
        assert_eq!(slot_tone().as_deref(), Some(&b"redeemed"[..]), "B's tone is the slot's now");
        assert!(!a.contract_violation.load(Relaxed) && !b.contract_violation.load(Relaxed), "no contract violation");
    }

    /// (a) A unit evicted while the host closes is refused its reseat (`Closing`) and stays in its
    /// owner's hand: a stop releases it in order. (b) A stop with the unit still in the engine leaves
    /// the plugin loaded and the unit abandoned there, the device rendering on.
    #[test]
    fn stopping_an_owner_releases_a_unit_in_hand_and_leaks_one_still_in_the_engine() {
        let _one = engine_slot::one_engine_test_at_a_time();
        {
            let device = device(48_000);
            let _feed = FeedStandIn::start(device.host().clone());
            let obs = Arc::new(Observed::default());
            let (owner, answer) = seat(&device, 0, 31, &obs, None);
            assert!(matches!(answer, Answer::Seated { .. }), "{answer:?}");
            assert!(wait_for(|| obs.processes.load(Relaxed) > 8));

            device.host().set_closing();
            device.rebuild_at(48_000, 4096);
            // The eviction reactivates at the new engine's terms, then the resubmit is refused.
            assert!(wait_for(|| obs.activations.load(Relaxed) == 2), "the evicted unit reactivates");
            owner.stop().expect("a unit in hand is released");
            assert_eq!((obs.starts.load(Relaxed), obs.stops.load(Relaxed)), (1, 1), "never started again");
            assert_eq!(obs.deactivations.load(Relaxed), 2, "deactivated by the reactivate and by the release");
            assert!(released_in_order(&obs), "stop < deactivate < destroy");
            assert!(!obs.contract_violation.load(Relaxed), "no contract violation");
        }
        let device = device(48_000);
        let _feed = FeedStandIn::start(device.host().clone());
        let obs = Arc::new(Observed::default());
        let (owner, answer) = seat(&device, 0, 32, &obs, None);
        assert!(matches!(answer, Answer::Seated { .. }), "{answer:?}");
        assert!(wait_for(|| obs.processes.load(Relaxed) > 8));
        assert!(owner.stop().is_err(), "a unit still in the engine: the stop says so");
        assert_eq!(obs.deactivations.load(Relaxed), 0, "left loaded: never deactivated");
        assert_eq!(obs.destroy_seq.load(Relaxed), 0, "left loaded: never destroyed");
        let processed = obs.processes.load(Relaxed);
        assert!(device.wait_blocks(8), "the device renders on");
        assert!(wait_for(|| obs.processes.load(Relaxed) > processed + 4), "the abandoned unit still runs");
        assert!(!obs.contract_violation.load(Relaxed), "no contract violation");
    }

    /// Two instances of one plugin file on two owners (two ids, one per slot) share its entry: it
    /// stays initialised while either lives, and the last release deinitialises it.
    #[test]
    fn two_instances_of_one_plugin_file_on_two_owners_keep_their_entry_balanced() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let _feed = FeedStandIn::start(device.host().clone());
        let (inits, deinits) = (|| COUNTED_INITS.load(Relaxed), || COUNTED_DEINITS.load(Relaxed));
        assert_eq!(inits(), deinits(), "balanced before");

        let (first, second) = (Arc::new(Observed::default()), Arc::new(Observed::default()));
        let mut owners = Vec::new();
        for (slot, id, obs) in [(0, 41, &first), (1, 42, &second)] {
            let unit_host = device.host().unit(slot, id, Spec::Plugin(identity()), 0, false).unwrap();
            let (owner, answer) = unit_owner::spawn_unit(
                PluginFormat::Clap,
                unit_host,
                0,
                Arc::new(|_| {}),
                None,
                entry_runner(&COUNTED_ENTRY, vec![obs.clone()]),
            )
            .expect("the unit seats");
            assert!(matches!(answer, Answer::Seated { .. }), "{answer:?}");
            owners.push(owner);
        }
        assert!(wait_for(|| first.processes.load(Relaxed) > 8 && second.processes.load(Relaxed) > 8), "both process");
        assert!(inits() > deinits(), "the entry is initialised while its instances live");

        let second_owner = owners.pop().unwrap();
        remove(&device, 41, owners.pop().unwrap()).expect("the first released in order");
        assert!(released_in_order(&first), "the first plugin destroyed");
        let processed = second.processes.load(Relaxed);
        assert!(wait_for(|| second.processes.load(Relaxed) > processed + 8), "the other still processes");
        assert!(inits() > deinits(), "the entry stays initialised for the other");

        remove(&device, 42, second_owner).expect("the second released in order");
        assert!(released_in_order(&second), "the second plugin destroyed");
        assert_eq!(inits(), deinits(), "the last release deinitialised the entry");
        assert!(!first.contract_violation.load(Relaxed) && !second.contract_violation.load(Relaxed));
    }

    #[test]
    fn a_param_set_through_the_unit_owner_reaches_the_plugin_and_an_unlisted_id_is_refused() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let _feed = FeedStandIn::start(device.host().clone());
        let obs = Arc::new(Observed::default());
        obs.param_count.store(2, Relaxed);
        let (owner, _) = seat(&device, 0, 51, &obs, None);
        let listed: Vec<u32> = owner.list_params().unwrap().iter().map(|p| p.id).collect();
        assert_eq!(listed, vec![100, 101]);
        assert!(owner.set_param(7, 0.1).is_err(), "an id the plugin never listed");
        owner.set_param(101, 0.75).unwrap();
        assert!(wait_for(|| obs.param_values.load(Relaxed) == 1), "the param reaches the processor");
        assert_eq!(obs.last_param.load(Relaxed), 101);
        assert_eq!(f64::from_bits(obs.last_param_value.load(Relaxed)), 0.75);
        remove(&device, 51, owner).expect("released in order");
    }

    /// A parked seat whose owner is stopped in the same turn its removal posts `Back::Dismissed`: the
    /// stop takes the ordered release (the plugin destroyed, its tone retired), never the leak.
    #[test]
    fn a_parked_unit_stopped_as_its_removal_dismisses_it_is_released_not_leaked() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let feed = FeedStandIn::start(device.host().clone());
        let dir = TempDir::new("unit-owner-stopped-dismiss");
        let ident = identity();
        let unit_id: UnitId = 61;
        let obs = Arc::new(Observed::default());
        obs.inputs.store(2, Relaxed);
        let (owner, answer) = seat(&device, 0, unit_id, &obs, Some(dir.unit_binding(unit_id, 0, ident.clone())));
        assert!(matches!(answer, Answer::Seated { .. }), "{answer:?}");
        assert!(wait_for(|| obs.processes.load(Relaxed) > 8));
        *obs.saved.lock().unwrap() = b"parked-tone".to_vec();
        owner.take_tone().expect("take_tone");

        obs.latency.store(lf_engine::max_chain_latency(48_000) as u32 + 1, Relaxed);
        request_restart(&obs);
        assert!(
            wait_for(|| feed.plate(0, unit_id).is_some_and(|p| p.state == PlateState::Failed && p.parked)),
            "the refused reseat parks"
        );

        // Hold the owner inside a save while it is told to stop and the removal posts `Dismissed`:
        // its next check of `running` ends the loop with the dismissal still in the mailbox.
        let (there_tx, there) = std::sync::mpsc::channel();
        let (go, go_rx) = std::sync::mpsc::channel::<()>();
        *obs.on_save.lock().unwrap() = Some(Box::new(move || {
            let _ = there_tx.send(());
            let _ = go_rx.recv_timeout(PATIENCE);
        }));
        std::thread::scope(|s| {
            let saving = s.spawn(|| owner.take_tone());
            there.recv_timeout(PATIENCE).expect("the owner is inside the save");
            owner.stop_serving();
            let Removal::Sent(op) = device.host().remove_unit(unit_id).expect("remove_unit") else {
                panic!("a parked unit's removal is sent");
            };
            assert_eq!(device.host().wait_op(op, PATIENCE).expect("wait_op"), OpState::Applied);
            go.send(()).unwrap();
            saving.join().unwrap().expect("the save answers");
        });
        owner.stop().expect("released in order, not leaked");
        assert_eq!(obs.deactivations.load(Relaxed), 2, "deactivated by the reactivate and by the release");
        assert!(obs.destroy_seq.load(Relaxed) > 0, "destroyed");
        assert!(wait_for(|| feed.plate(0, unit_id).is_none()), "the plate is gone");
        let retired = dir.store().load(0, &ident).unwrap().expect("the slot's tone");
        assert_eq!(retired.state, b"parked-tone", "the dismissed unit's tone retired into the slot's file");
        assert!(!obs.contract_violation.load(Relaxed), "no contract violation");
    }

    /// Finding 1: a first install applies, then a rebuild evicts it before the owner polls `Applied`.
    /// The reseat is refused: the unit had been seated, so the plate is parked, the unit kept, and
    /// the plugin not destroyed.
    #[test]
    fn a_first_install_evicted_before_poll_parks_when_reseat_is_refused() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = TestDevice::start(48_000, 256, Duration::from_millis(2), |_| 0.1);
        let ident = identity();
        let unit_id: UnitId = 71;

        // Mark feed fresh so `UnitHost::poll` does not drain the event ring while we stage the
        // eviction before the owner ever sees `OpState::Applied`.
        device.host().drain_feed(&mut Vec::new());

        let obs = Arc::new(Observed::default());
        obs.inputs.store(2, Relaxed);
        obs.latency.store(0, Relaxed);
        obs.param_count.store(2, Relaxed);

        let unit_host = device.host().unit(0, unit_id, Spec::Plugin(ident), 0, false).unwrap();
        let obs_t = obs.clone();
        let answered = Arc::new(AtomicBool::new(false));
        let answered_t = answered.clone();
        let spawner = std::thread::spawn(move || {
            let res = unit_owner::spawn_unit(
                PluginFormat::Clap,
                unit_host,
                0,
                Arc::new(|_| {}),
                None,
                unit_runner(vec![obs_t], "bleeploop.restart-fixture", None),
            );
            answered_t.store(true, Release);
            res
        });

        // Wait for the first install to apply and process in the old engine, still unpolled.
        assert!(wait_for(|| obs.processes.load(Relaxed) > 0), "first install applied and processed");
        assert!(!answered.load(Acquire), "owner had not polled Applied before rebuild");

        // Make the reseat on the rebuilt engine refuse on latency, then rebuild to evict.
        obs.latency.store(lf_engine::max_chain_latency(44_100) as u32 + 1, Relaxed);
        device.rebuild_at(44_100, 256);

        let feed = FeedStandIn::start(device.host().clone());
        let (handle, answer) = spawner.join().unwrap().expect("evicted-after-apply unit stays alive, not failed");
        assert!(matches!(answer, Answer::Seated { .. }), "expected Seated answer, got {answer:?}");
        assert!(
            wait_for(|| feed.plate(0, unit_id).is_some_and(|p| p.state == PlateState::Failed && p.parked)),
            "refused reseat parks the plate"
        );
        assert!(!handle.is_finished(), "owner keeps the unit parked");
        assert_eq!(obs.destroy_seq.load(Relaxed), 0, "plugin not destroyed");
        let params = handle.list_params().expect("parked unit keeps serving list_params");
        assert_eq!(params.iter().map(|p| p.id).collect::<Vec<_>>(), vec![100, 101]);

        assert_eq!(
            device.host().remove_unit(unit_id).expect("remove_unit"),
            Removal::Plate,
            "evicted unit parked out of the new engine removes as Plate and posts Dismissed"
        );
        assert!(wait_for(|| handle.is_finished()));
        handle.stop().expect("owner released cleanly");
        assert!(obs.destroy_seq.load(Relaxed) > 0, "destroyed on dismissal");
        assert!(!obs.contract_violation.load(Relaxed), "no contract violation");
    }

    /// Finding 2: with the device closed (engine and port alive, no feed ticking), a seated unit's
    /// plugin-asked restart settles its withdrawal and reseats the unit.
    #[test]
    fn a_restart_with_the_device_closed_and_no_feed_settles_and_reseats() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let mut device = device(48_000);
        let ident = identity();
        let unit_id: UnitId = 72;

        let obs = Arc::new(Observed::default());
        obs.inputs.store(2, Relaxed);
        obs.latency.store(0, Relaxed);

        let unit_host = device.host().unit(0, unit_id, Spec::Plugin(ident), 0, false).unwrap();
        let (handle, answer) = unit_owner::spawn_unit(
            PluginFormat::Clap,
            unit_host,
            0,
            Arc::new(|_| {}),
            None,
            unit_runner(vec![obs.clone()], "bleeploop.restart-fixture", None),
        )
        .expect("unit seats without a feed");
        assert!(matches!(answer, Answer::Seated { .. }), "{answer:?}");
        assert!(wait_for(|| obs.processes.load(Relaxed) > 4), "unit processes while device runs");

        // Close the device: engine and port stay alive, no render thread, no feed ticking.
        device.stop();
        obs.latency.store(64, Relaxed);
        request_restart(&obs);

        // Bound wait tightly enough to fail quickly when the withdraw op is dropped.
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if obs.activations.load(Relaxed) == 2
                && obs.deactivations.load(Relaxed) == 1
                && installed(&device, 0) == Some((SlotKind::Effect, 64))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(obs.deactivations.load(Relaxed), 1, "withdrawal settled while device was closed");
        assert_eq!(obs.activations.load(Relaxed), 2, "unit reactivated while device was closed");
        assert_eq!(installed(&device, 0), Some((SlotKind::Effect, 64)), "unit reseated at new latency");

        // Reopen the device: the reseated unit starts processing again.
        let before = obs.processes.load(Relaxed);
        device.resume();
        assert!(wait_for(|| obs.starts.load(Relaxed) == 2 && obs.processes.load(Relaxed) > before + 4));

        remove(&device, unit_id, handle).expect("owner released cleanly");
    }

    /// Finding 4: the owner's first answer must not block when the caller has not yet received on
    /// `ready_rx` (capacity-1 channel + non-blocking send).
    #[test]
    fn the_first_answer_does_not_block_the_owner_when_caller_has_not_received() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let feed = FeedStandIn::start(device.host().clone());
        let ident = identity();
        let unit_id: UnitId = 76;

        let obs = Arc::new(Observed::default());
        obs.inputs.store(2, Relaxed);
        obs.param_count.store(2, Relaxed);

        let unit_host = device.host().unit(0, unit_id, Spec::Plugin(ident), 0, false).unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let claim = Arc::new(engine_slot::LoadClaim::default());
        let edited = Arc::new(AtomicBool::new(false));
        let (_params_tx, params) = RingBuffer::new(64);
        let params_tx = Arc::new(Mutex::new(_params_tx));
        let (request_tx, requests) = std::sync::mpsc::channel();
        let (ready_tx, ready_rx) = unit_owner::ready_channel();
        let param_ids = super::ParamIds::default();
        let shared_name = Arc::new(std::sync::OnceLock::new());
        let ctx = UnitCtx {
            unit: unit_host,
            running: running.clone(),
            claim,
            requests,
            params,
            params_tx,
            param_ids,
            sink: Arc::new(|_| {}),
            editor_parent: 0,
            ready: ready_tx,
            tone: super::super::super::tone::ToneKeeper::new(None, edited),
            shared_name,
        };
        let obs_t = obs.clone();
        let thread = std::thread::spawn(move || unit_runner(vec![obs_t], "bleeploop.restart-fixture", None)(ctx));

        // Do NOT call `ready_rx.recv()` yet: wait until the install is settled `Applied` in the model.
        assert!(
            wait_for(|| feed.plate(0, unit_id).is_some_and(|p| p.state == PlateState::Seated)),
            "unit install settled as Seated"
        );

        // Two sequential round-trips guarantee at least one full owner loop turn ran after `Applied`
        // (so `send_answer` definitely executed) while `ready_rx` is still unreceived.
        let ask = || {
            let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
            request_tx.send(super::OwnerRequest::ListParams(reply_tx)).unwrap();
            reply_rx.recv_timeout(Duration::from_secs(2))
        };
        let first = ask();
        let second = ask();
        let answer = ready_rx.try_recv();
        let Removal::Sent(rm_op) = device.host().remove_unit(unit_id).unwrap() else { panic!() };
        assert_eq!(device.host().wait_op(rm_op, PATIENCE).unwrap(), OpState::Applied);
        assert!(wait_for(|| thread.is_finished()), "owner tears down on Back::Removed");
        running.store(false, Release);
        let _ = request_tx.send(super::OwnerRequest::Wake);
        thread.join().unwrap().expect("owner ended cleanly");

        let params1 = first.expect("first list_params must not block on ready_tx").expect("list_params 1");
        let params2 = second.expect("second list_params must not block on ready_tx").expect("list_params 2");
        assert_eq!(params1.iter().map(|p| p.id).collect::<Vec<_>>(), vec![100, 101]);
        assert_eq!(params2.iter().map(|p| p.id).collect::<Vec<_>>(), vec![100, 101]);
        assert!(matches!(answer, Ok(Ok(Answer::Seated { .. }))), "buffered answer: {answer:?}");
    }

    /// Finding 5: when spawning the owner thread fails, `spawn_unit` fails the plate and releases
    /// the reservation instead of leaking both.
    #[test]
    fn a_thread_spawn_failure_fails_the_plate_and_releases_the_reservation() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let feed = FeedStandIn::start(device.host().clone());
        let ident = identity();
        let unit_id: UnitId = 77;

        let unit_host = device.host().unit(0, unit_id, Spec::Plugin(ident.clone()), 0, false).unwrap();
        let err = match unit_owner::spawn_unit_with(
            PluginFormat::Clap,
            unit_host,
            0,
            Arc::new(|_| {}),
            None,
            |_| Ok(()),
            |_, _| Err(std::io::Error::other("simulated OS thread limit")),
        ) {
            Err(e) => e,
            Ok((_, ans)) => panic!("expected spawn error, got {ans:?}"),
        };
        assert!(err.contains("spawn unit owner thread"), "err: {err}");
        assert!(
            wait_for(|| feed.plate(0, unit_id).is_some_and(|p| p.state == PlateState::Failed && !p.parked)),
            "plate must read failed after thread spawn error, got {:?}",
            feed.plate(0, unit_id)
        );
        // The id reservation must be free so the same id can be reserved again.
        let retry = device
            .host()
            .unit(0, unit_id, Spec::Plugin(ident), 0, false)
            .expect("id reservation must be released after spawn error");
        retry.failed("cleanup".to_string());
        retry.release();
    }

    /// Round D3 (a), Sol's first half at owner level: while a reload candidate stands for A, A's
    /// plugin-asked restart is deferred (`Reloading`); the candidate submits behind another unit's
    /// held install in a take and is refused when the take ends. A's owner stays alive, the plate
    /// stays `Seated`, and A runs its deferred restart and reseats.
    #[test]
    fn a_failed_reload_behind_a_held_install_leaves_the_original_owner_alive_and_runs_its_deferred_restart() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let feed = FeedStandIn::start(device.host().clone());
        let ident_a = identity();
        let ident_b = ToneIdentity {
            format: "clap".to_string(),
            path: "dummy-b.clap".to_string(),
            id: "bleeploop.dummy-b".to_string(),
        };
        let unit_a: UnitId = 81;
        let unit_b: UnitId = 82;

        let obs = Arc::new(Observed::default());
        obs.inputs.store(2, Relaxed);
        obs.latency.store(0, Relaxed);
        obs.param_count.store(2, Relaxed);

        let (handle, answer) = seat(&device, 0, unit_a, &obs, None);
        assert!(matches!(answer, Answer::Seated { .. }), "{answer:?}");
        assert!(wait_for(|| obs.processes.load(Relaxed) > 8), "A processes");

        start_take(&device);

        // Unit B's install (latency 64) is held at the head of slot 0's port during the take.
        let b_host = device.host().unit(0, unit_b, Spec::Plugin(ident_b), 1, false).unwrap();
        b_host.activated(Activated { name: "Held B".to_string(), source: false, latency: 64 });
        let b_terms = b_host.terms().expect("engine open");
        let b_op = b_host
            .submit(Box::new(FixedLatencyUnit { latency: 64 }), b_terms)
            .map_err(|(_, e)| e)
            .expect("submit B");
        assert!(wait_for(|| b_host.poll(b_op) == OpState::Held && feed.chain(0).held), "B is held");

        // Reserve a reload candidate for A, then ask A's plugin to restart: deferred (`Reloading`).
        let cand_host = device.host().reload_unit(unit_a, ident_a).expect("reload_unit");
        assert!(wait_for(|| feed.plate(0, unit_a).is_some_and(|p| p.reloading)), "plate shows reloading");

        obs.latency.store(128, Relaxed);
        request_restart(&obs);
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(handle.list_params().expect("A answers while reload stands").len(), 2);
        assert_eq!(obs.deactivations.load(Relaxed), 0, "A's withdraw is deferred while candidate stands");
        assert_eq!(obs.activations.load(Relaxed), 1);
        assert!(!handle.is_finished(), "A's owner stays alive");

        // Candidate submits behind B's held install with a latency the engine will refuse on settle.
        let bad_latency = lf_engine::max_chain_latency(48_000) + 1;
        cand_host.activated(Activated {
            name: "Refused reload".to_string(),
            source: false,
            latency: bad_latency,
        });
        let cand_terms = cand_host.terms().expect("engine open");
        let cand_op = cand_host
            .submit(Box::new(FixedLatencyUnit { latency: bad_latency }), cand_terms)
            .map_err(|(_, e)| e)
            .expect("submit candidate");
        assert_eq!(cand_host.poll(cand_op), OpState::Pending, "candidate waits behind B's held install");
        assert_eq!(obs.deactivations.load(Relaxed), 0, "A still seated while candidate is queued");

        let processed_before = obs.processes.load(Relaxed);
        stop_take(&device);

        assert!(wait_for(|| b_host.poll(b_op) == OpState::Applied), "B applies after stop_take");
        assert!(
            wait_for(|| matches!(cand_host.poll(cand_op), OpState::Refused(_))),
            "candidate is refused when the take ends"
        );
        assert!(
            wait_for(|| matches!(cand_host.take_back(), Some(Back::Refused { .. }))),
            "candidate receives Back::Refused"
        );
        cand_host.release();

        assert!(
            wait_for(|| obs.deactivations.load(Relaxed) == 1
                && obs.activations.load(Relaxed) == 2
                && obs.starts.load(Relaxed) == 2
                && obs.processes.load(Relaxed) > processed_before + 8
                && feed.plate(0, unit_a).is_some_and(|p| {
                    p.state == PlateState::Seated && !p.reloading && !p.held && p.latency == 128
                })),
            "A stays alive, runs its deferred restart, and reseats at latency 128"
        );
        assert!(!handle.is_finished(), "A's owner is still alive");
        assert_eq!(handle.list_params().expect("A still serves after reseat").len(), 2);

        let Removal::Sent(b_rm) = device.host().remove_unit(unit_b).expect("remove B") else {
            panic!("seated unit B removal must be Sent");
        };
        assert_eq!(device.host().wait_op(b_rm, PATIENCE).expect("wait_op B"), OpState::Applied);
        assert!(wait_for(|| matches!(b_host.take_back(), Some(Back::Removed(_)))));
        b_host.release();

        remove(&device, unit_a, handle).expect("A released in order");
        assert!(!obs.contract_violation.load(Relaxed), "no contract violation");
        assert_eq!(obs.rt_calls_off_device.load(Relaxed), 0, "no RT call off the device thread");
    }

    /// Round D3 (b), Astra 1 at owner level: A asks a restart and `remove_unit` arrives while A is
    /// parked inside reactivate's `activate` (both settled immediately and held in a take). The
    /// owner keeps its unit on `SubmitError::Removing` without resubmitting, waits for `Dismissed`,
    /// retires the tone, and finishes with exactly one release.
    #[test]
    fn a_removal_during_reactivate_retires_the_tone_and_finishes_with_one_release() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let ident = identity();

        // Part 1: removal settles while A is parked inside reactivate.
        {
            let device = device(48_000);
            let feed = FeedStandIn::start(device.host().clone());
            let dir = TempDir::new("unit-owner-remove-reactivating-settled");
            let unit_id: UnitId = 83;

            let obs = Arc::new(Observed::default());
            obs.inputs.store(2, Relaxed);
            obs.latency.store(0, Relaxed);

            let (handle, answer) =
                seat(&device, 0, unit_id, &obs, Some(dir.unit_binding(unit_id, 0, ident.clone())));
            assert!(matches!(answer, Answer::Seated { .. }), "{answer:?}");
            assert!(wait_for(|| obs.processes.load(Relaxed) > 8));

            *obs.saved.lock().unwrap() = b"reactivating-tone-v1".to_vec();
            handle.take_tone().expect("take_tone");
            let unit_file = dir.0.join(ToneIdentity::unit_file_name(unit_id));
            assert!(unit_file.exists(), "unit tone file exists before restart");

            let (there_tx, there_rx) = std::sync::mpsc::channel();
            let (go_tx, go_rx) = std::sync::mpsc::channel();
            *obs.park_in_activate.lock().unwrap() = Some((there_tx, go_rx));

            request_restart(&obs);
            there_rx.recv_timeout(PATIENCE).expect("owner paused inside reactivate's activate");
            assert_eq!(obs.stops.load(Relaxed), 1);
            assert_eq!(obs.deactivations.load(Relaxed), 1);
            assert_eq!(obs.activations.load(Relaxed), 2);

            let Removal::Sent(rm_op) = device.host().remove_unit(unit_id).expect("remove_unit") else {
                panic!("withdrawn seat removal must be Sent");
            };
            assert_eq!(device.host().wait_op(rm_op, PATIENCE).expect("wait_op"), OpState::Applied);

            go_tx.send(()).unwrap();
            assert!(wait_for(|| handle.is_finished()), "owner finishes on Dismissed after Removing");
            handle.stop().expect("owner ended cleanly");

            assert!(!unit_file.exists(), "unit tone file retired on removal during reactivate");
            let retired = dir
                .store()
                .load(0, &ident)
                .expect("load slot tone")
                .expect("slot tone holds retired state");
            assert_eq!(retired.state, b"reactivating-tone-v1");
            assert_eq!((obs.starts.load(Relaxed), obs.stops.load(Relaxed)), (1, 1));
            assert_eq!(obs.activations.load(Relaxed), 2);
            assert_eq!(obs.deactivations.load(Relaxed), 2, "deactivated by reactivate and by single release");
            assert!(released_in_order(&obs), "stop < deactivate < destroy");
            assert!(wait_for(|| feed.plate(0, unit_id).is_none()), "plate is gone");
            assert!(!obs.contract_violation.load(Relaxed), "no contract violation");
            assert_eq!(obs.rt_calls_off_device.load(Relaxed), 0, "no RT call off the device thread");
        }

        // Part 2: removal is held in a take while A reactivates; after `Removing` the owner waits
        // without resubmitting until the take ends and `Dismissed` arrives.
        {
            let device = device(48_000);
            let feed = FeedStandIn::start(device.host().clone());
            let dir = TempDir::new("unit-owner-remove-reactivating-held");
            let unit_id: UnitId = 84;

            let obs = Arc::new(Observed::default());
            obs.inputs.store(2, Relaxed);
            obs.latency.store(64, Relaxed);
            obs.param_count.store(2, Relaxed);

            let (handle, answer) =
                seat(&device, 0, unit_id, &obs, Some(dir.unit_binding(unit_id, 0, ident.clone())));
            assert!(matches!(answer, Answer::Seated { .. }), "{answer:?}");
            assert!(wait_for(|| obs.processes.load(Relaxed) > 8));

            *obs.saved.lock().unwrap() = b"reactivating-held-tone".to_vec();
            handle.take_tone().expect("take_tone");
            let unit_file = dir.0.join(ToneIdentity::unit_file_name(unit_id));
            assert!(unit_file.exists(), "unit tone file exists before restart");

            start_take(&device);

            let (there_tx, there_rx) = std::sync::mpsc::channel();
            let (go_tx, go_rx) = std::sync::mpsc::channel();
            *obs.park_in_activate.lock().unwrap() = Some((there_tx, go_rx));

            request_restart(&obs);
            there_rx.recv_timeout(PATIENCE).expect("owner paused inside reactivate's activate");

            let Removal::Sent(rm_op) = device.host().remove_unit(unit_id).expect("remove_unit") else {
                panic!("withdrawn seat removal must be Sent");
            };
            assert!(
                wait_for(|| feed.plate(0, unit_id).is_some_and(|p| p.state == PlateState::Leaving && p.held)),
                "removal is held during the take"
            );

            go_tx.send(()).unwrap();
            // After `submit` returns `Removing`, the owner waits for the terminal without exiting
            // early or reactivating again even if another restart is requested.
            request_restart(&obs);
            std::thread::sleep(Duration::from_millis(80));
            assert_eq!(handle.list_params().expect("owner answers while awaiting removal").len(), 2);
            assert!(!handle.is_finished(), "owner waits for the held removal to settle");
            assert_eq!(obs.activations.load(Relaxed), 2, "no extra activation while awaiting removal");
            assert_eq!(obs.deactivations.load(Relaxed), 1, "not released until removal settles");

            stop_take(&device);
            assert_eq!(device.host().wait_op(rm_op, PATIENCE).expect("wait_op"), OpState::Applied);
            assert!(wait_for(|| handle.is_finished()), "owner finishes once held removal settles");
            handle.stop().expect("owner ended cleanly");

            assert!(!unit_file.exists(), "unit tone file retired once held removal settled");
            let retired = dir
                .store()
                .load(0, &ident)
                .expect("load slot tone")
                .expect("slot tone holds retired state");
            assert_eq!(retired.state, b"reactivating-held-tone");
            assert_eq!((obs.starts.load(Relaxed), obs.stops.load(Relaxed)), (1, 1));
            assert_eq!(obs.activations.load(Relaxed), 2);
            assert_eq!(obs.deactivations.load(Relaxed), 2, "deactivated by reactivate and by single release");
            assert!(released_in_order(&obs), "stop < deactivate < destroy");
            assert!(wait_for(|| feed.plate(0, unit_id).is_none()), "plate is gone");
            assert!(!obs.contract_violation.load(Relaxed), "no contract violation");
            assert_eq!(obs.rt_calls_off_device.load(Relaxed), 0, "no RT call off the device thread");
        }
    }

    /// Round D3 (c), stop path: withdraw → held removal in a take → the withdrawn box is held in
    /// `Leave::Pending` → stop the owner: teardown drains via `take_back_for_shutdown()` and takes
    /// the ordered release (never `abandon_in_engine`), keeping the tone file unretired and keeping
    /// the id reserved until the held removal settles.
    #[test]
    fn stopping_an_owner_with_a_withdrawn_box_held_for_removal_releases_in_order_and_keeps_id_reserved() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let feed = FeedStandIn::start(device.host().clone());
        let dir = TempDir::new("unit-owner-stop-held-remove");
        let ident = identity();
        let unit_id: UnitId = 85;

        let obs = Arc::new(Observed::default());
        obs.inputs.store(2, Relaxed);
        obs.latency.store(64, Relaxed);
        obs.param_count.store(2, Relaxed);

        let (handle, answer) =
            seat(&device, 0, unit_id, &obs, Some(dir.unit_binding(unit_id, 0, ident.clone())));
        assert!(matches!(answer, Answer::Seated { .. }), "{answer:?}");
        assert!(wait_for(|| obs.processes.load(Relaxed) > 8));

        *obs.saved.lock().unwrap() = b"held-remove-tone".to_vec();
        handle.take_tone().expect("take_tone");
        let unit_file = dir.0.join(ToneIdentity::unit_file_name(unit_id));
        assert!(unit_file.exists(), "unit tone file exists");

        // Pause the owner inside save #1, stage a take + restart + save #2 so the owner sends
        // `withdraw()` and then pauses inside save #2 before its next `take_back()`.
        let (there1_tx, there1_rx) = std::sync::mpsc::channel();
        let (go1_tx, go1_rx) = std::sync::mpsc::channel::<()>();
        *obs.on_save.lock().unwrap() = Some(Box::new(move || {
            let _ = there1_tx.send(());
            let _ = go1_rx.recv_timeout(PATIENCE);
        }));

        let rm_op = std::thread::scope(|s| {
            let saving1 = s.spawn(|| handle.take_tone());
            there1_rx.recv_timeout(PATIENCE).expect("owner paused in save #1");

            start_take(&device);
            request_restart(&obs);

            let (there2_tx, there2_rx) = std::sync::mpsc::channel();
            let (go2_tx, go2_rx) = std::sync::mpsc::channel::<()>();
            *obs.on_save.lock().unwrap() = Some(Box::new(move || {
                let _ = there2_tx.send(());
                let _ = go2_rx.recv_timeout(PATIENCE);
            }));
            let save2_rx = handle.start_tone_save().expect("enqueue save #2");

            go1_tx.send(()).unwrap();
            there2_rx.recv_timeout(PATIENCE).expect("owner sent withdraw and paused in save #2");

            let Removal::Sent(rm_op) = device.host().remove_unit(unit_id).expect("remove_unit") else {
                panic!("withdrawn unit removal must be Sent");
            };
            assert!(
                wait_for(|| obs.stops.load(Relaxed) == 1
                    && feed.plate(0, unit_id).is_some_and(|p| p.state == PlateState::Leaving && p.held)),
                "withdraw applied and removal is held with the withdrawn box held in Leave::Pending"
            );

            go2_tx.send(()).unwrap();
            saving1.join().unwrap().expect("save #1 answered");
            save2_rx.recv_timeout(PATIENCE).expect("save #2 answered").expect("save #2 ok");
            rm_op
        });

        // Owner ran its normal loop again (`take_back()` returned None while removal is held).
        assert_eq!(handle.list_params().expect("owner still alive while removal is held").len(), 2);
        assert!(!handle.is_finished(), "owner does not exit while box is held");
        assert_eq!(obs.deactivations.load(Relaxed), 0, "withdrawn box is hidden from normal take_back");
        assert!(
            device.host().unit(0, unit_id, Spec::Plugin(ident.clone()), 0, false).is_err(),
            "id is reserved while held removal stands"
        );

        // Stop the owner: must claim the held box via `take_back_for_shutdown()` and take ordered release.
        handle
            .stop()
            .expect("stop claims held box for ordered release instead of abandon_in_engine");
        assert_eq!((obs.starts.load(Relaxed), obs.stops.load(Relaxed)), (1, 1));
        assert_eq!(obs.deactivations.load(Relaxed), 1, "deactivated once by ordered release");
        assert!(released_in_order(&obs), "stop < deactivate < destroy");
        assert!(unit_file.exists(), "shutdown with held removal keeps unit tone file (retire=false)");
        assert!(
            dir.store().load(0, &ident).expect("load slot tone").is_none(),
            "slot tone not retired on shutdown with held removal"
        );
        assert!(
            device.host().unit(0, unit_id, Spec::Plugin(ident.clone()), 0, false).is_err(),
            "id stays reserved after owner release while held removal still stands"
        );

        stop_take(&device);
        assert_eq!(device.host().wait_op(rm_op, PATIENCE).expect("wait_op"), OpState::Applied);
        assert!(wait_for(|| feed.plate(0, unit_id).is_none()), "plate gone once removal settles");
        let fresh = device
            .host()
            .unit(0, unit_id, Spec::Plugin(ident), 0, false)
            .expect("id free once held removal settles");
        fresh.failed("cleanup".to_string());
        fresh.release();
        assert!(!obs.contract_violation.load(Relaxed), "no contract violation");
        assert_eq!(obs.rt_calls_off_device.load(Relaxed), 0, "no RT call off the device thread");
    }
}
