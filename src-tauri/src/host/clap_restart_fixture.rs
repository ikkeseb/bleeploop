//! A raw CLAP plugin with real activate/process callbacks, loaded by clack with the production
//! `LfHost` and driven by the production RT producer (`producer_loop`) against a plain heap ring in
//! place of the WebView2 SharedBuffer. Proves the plugin-initiated paths: `host.request_restart`
//! (deactivate → activate → RT resumed, on the right threads) and `clap_host_params.rescan`; and,
//! through its `clap.params` extension, that a malformed parameter count is refused. The engine-mode
//! tests at the end load the same plugin through `engine_slot` into a test device's engine.
//! No audio device or GUI required.
use super::super::transport::HOP1_HEADER_BYTES;
use super::*;
use clap_sys::{
    entry::clap_plugin_entry,
    events::{
        clap_event_header, clap_event_note, clap_event_param_value, CLAP_CORE_EVENT_SPACE_ID,
        CLAP_EVENT_NOTE_OFF, CLAP_EVENT_NOTE_ON, CLAP_EVENT_PARAM_VALUE,
    },
    ext::audio_ports::{clap_audio_port_info, clap_plugin_audio_ports, CLAP_EXT_AUDIO_PORTS},
    ext::latency::{clap_plugin_latency, CLAP_EXT_LATENCY},
    ext::params::{
        clap_host_params, clap_param_info, clap_plugin_params, CLAP_EXT_PARAMS,
        CLAP_PARAM_RESCAN_VALUES,
    },
    factory::plugin_factory::clap_plugin_factory,
    host::clap_host,
    id::{clap_id, CLAP_INVALID_ID},
    plugin::{clap_plugin, clap_plugin_descriptor},
    process::{clap_process, clap_process_status, CLAP_PROCESS_CONTINUE},
    string_sizes::{CLAP_NAME_SIZE, CLAP_PATH_SIZE},
    version::CLAP_VERSION,
};
use std::cell::RefCell;
use std::ffi::{c_char, c_void, CStr};
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
    /// What the next instance created on this thread records into; a fresh `Observed` when unset.
    /// An engine-mode test sets it from the owner thread, just before that thread instantiates.
    static OBSERVE: RefCell<Option<Arc<Observed>>> = const { RefCell::new(None) };
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
    /// What `clap.latency` reports.
    latency: AtomicU32,
    /// The input port's channel count (`clap.audio-ports`); 0 = no input port, an instrument. With
    /// an input the plugin is an effect that echoes each input channel to its output.
    inputs: AtomicU32,
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
    CLAP_PROCESS_CONTINUE
}

unsafe extern "C" fn reset(_: *const clap_plugin) {}

unsafe extern "C" fn on_main_thread(_: *const clap_plugin) {}

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
    if index >= unsafe { state(plugin) }.param_count.load(Relaxed).min(16) {
        return false;
    }
    // SAFETY: the host passes a writable clap_param_info; every field is written.
    unsafe {
        info.write(clap_param_info {
            id: 100 + index,
            flags: 0,
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

unsafe extern "C" fn port_count(plugin: *const clap_plugin, is_input: bool) -> u32 {
    if is_input {
        (unsafe { state(plugin) }.inputs.load(Relaxed) > 0) as u32
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
    let inputs = unsafe { state(plugin) }.inputs.load(Relaxed);
    if index != 0 || (is_input && inputs == 0) {
        return false;
    }
    // SAFETY: the host passes a writable clap_audio_port_info; every field is written.
    unsafe {
        info.write(clap_audio_port_info {
            id: is_input as u32,
            name: [0; CLAP_NAME_SIZE],
            flags: 0,
            channel_count: if is_input { inputs } else { 2 },
            port_type: std::ptr::null(),
            in_place_pair: CLAP_INVALID_ID,
        });
    }
    true
}

static AUDIO_PORTS: clap_plugin_audio_ports = clap_plugin_audio_ports { count: Some(port_count), get: Some(port_get) };

static PARAMS: clap_plugin_params = clap_plugin_params {
    count: Some(param_count),
    get_info: Some(param_info),
    get_value: Some(param_value),
    value_to_text: None,
    text_to_value: None,
    flush: None,
};

unsafe extern "C" fn extension(_: *const clap_plugin, id: *const c_char) -> *const c_void {
    let id = unsafe { CStr::from_ptr(id) };
    if id == CLAP_EXT_PARAMS {
        (&PARAMS as *const clap_plugin_params).cast()
    } else if id == CLAP_EXT_LATENCY {
        (&LATENCY as *const clap_plugin_latency).cast()
    } else if id == CLAP_EXT_AUDIO_PORTS {
        (&AUDIO_PORTS as *const clap_plugin_audio_ports).cast()
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
    let obs = OBSERVE.with(|o| o.borrow_mut().take()).unwrap_or_default();
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
            callback_requested: AtomicBool::new(false),
            restart_requested: AtomicBool::new(false),
        },
        |_| LfMain::default(),
        &entry,
        c"bleeploop.restart-fixture",
        &info,
    )
    .unwrap()
}

/// Spin until `pred` holds or `ms` elapse; returns whether it held.
fn wait_for(ms: u64, pred: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < deadline {
        if pred() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    pred()
}

#[test]
fn plugin_requested_restart_cycles_activation_on_the_owner_without_reload() {
    const CAP_FRAMES: u32 = 1024; // power of two (the hop-1 ring masks)
    const MAX_FRAMES: u32 = 512;
    const RATE: f64 = 48_000.0;

    let mut instance = fixture_instance();
    let plugin = instance.raw_instance() as *const clap_plugin;
    let s = unsafe { state(plugin) };

    // A plain heap ring stands in for the WebView2 SharedBuffer: header + f32 data, 4-byte aligned.
    let mut ring = vec![0u32; HOP1_HEADER_BYTES / 4 + CAP_FRAMES as usize];
    let cfg = RtConfig {
        slot: 0,
        shared_ptr: ring.as_mut_ptr() as usize,
        cap_frames: CAP_FRAMES,
        out_channels: 2,
        in_channels: 0,
        max_frames: MAX_FRAMES,
        sample_rate: RATE,
        device_rate: RATE,
    };
    let diag = Arc::new(ProducerDiag::new());
    diag.init(RATE, RATE, MAX_FRAMES, 2, CAP_FRAMES as usize, 256, 1);

    let stopped = instance.activate(|_, _| (), cfg.audio_configuration()).unwrap();
    let (_event_tx, event_rx) = RingBuffer::<PluginEvent>::new(16);
    let (_in_tx, in_rx) = RingBuffer::<f32>::new(16);
    let (mon_tx, _mon_rx) = RingBuffer::<f32>::new(16);
    let mut rt_guard = Some(
        spawn_rt(
            &cfg,
            stopped,
            RtRings {
                event_rx,
                in_rx,
                mon_tx,
            },
            128,
            BLOCK_CONFIG_GEN.load(Acquire),
            0.0,
            diag.clone(),
        )
        .unwrap(),
    );
    assert!(
        wait_for(2000, || s.processes.load(Relaxed) > 4),
        "the production RT loop must process the fixture"
    );
    assert_eq!(s.activations.load(Relaxed), 1);
    assert_eq!(s.starts.load(Relaxed), 1);

    // Stand in for the JS consumer (the worklet): keep the header read cursor ([1]) caught up with the write cursor
    // ([0]) until the producer is well past one ring capacity, so a restart that forgot the cursor
    // would wrap `used` and drop every block (the 2026-09-10 runtime finding).
    // SAFETY: the header words are 4-byte aligned; the RT thread is the sole writer of [0] and the
    // sole reader of [1], mirroring the WebView2 contract.
    let write_idx = unsafe { AtomicU32::from_ptr(ring.as_mut_ptr()) };
    let read_idx = unsafe { AtomicU32::from_ptr(ring.as_mut_ptr().add(1)) };
    assert!(
        wait_for(4000, || {
            let w = write_idx.load(Acquire);
            read_idx.store(w, Release);
            w > CAP_FRAMES * 2
        }),
        "the producer must publish past one ring capacity with a draining reader"
    );
    let written_before_restart = write_idx.load(Acquire);
    let dropped_before_restart = diag.frames_dropped.load(Relaxed);

    // The request comes in through the real host ABI from a foreign thread (a plugin's worker or
    // audio thread), and must allocate nothing under the RT alloc guard.
    let host = unsafe { *s.host };
    std::thread::spawn(move || {
        #[cfg(debug_assertions)]
        let allocations_before = super::super::rt_alloc::RT_ALLOCS.load(Relaxed);
        {
            #[cfg(debug_assertions)]
            let _guard = super::super::rt_alloc::guard();
            unsafe { (host.request_restart.unwrap())(&host) };
        }
        #[cfg(debug_assertions)]
        assert_eq!(
            super::super::rt_alloc::RT_ALLOCS.load(Relaxed),
            allocations_before,
            "request_restart through the real host ABI must allocate nothing"
        );
    })
    .join()
    .unwrap();
    assert_eq!(
        s.deactivations.load(Relaxed),
        0,
        "the request itself must not run foreign lifecycle code inline"
    );

    // Owner turn: the flag drains exactly once, and the cycle runs.
    assert!(instance.access_shared_handler(|sh| sh.restart_requested.swap(false, Acquire)));
    assert!(!instance.access_shared_handler(|sh| sh.restart_requested.swap(false, Acquire)));
    let processed_before = s.processes.load(Relaxed);
    service_restart(&mut instance, &mut rt_guard, &cfg, &diag).unwrap();
    assert!(rt_guard.is_some(), "a fresh RT producer replaces the joined one");
    assert_eq!(s.stops.load(Relaxed), 1, "the RT thread stopped processing before deactivate");
    assert_eq!(s.deactivations.load(Relaxed), 1);
    assert_eq!(s.activations.load(Relaxed), 2, "activate ran again on the same instance");
    assert!(
        wait_for(2000, || s.starts.load(Relaxed) == 2
            && s.processes.load(Relaxed) > processed_before + 4),
        "the respawned RT producer must resume processing"
    );
    assert!(instance.is_active());
    // The hop-1 write cursor continues from where the joined producer left it (the JS reader is
    // still at its old position), and — with the reader kept caught up — nothing is dropped.
    assert!(
        wait_for(2000, || {
            let w = write_idx.load(Acquire);
            read_idx.store(w, Release);
            w > written_before_restart + CAP_FRAMES
        }),
        "the respawned producer must continue the published write cursor, not restart it at 0"
    );
    assert_eq!(
        diag.frames_dropped.load(Relaxed),
        dropped_before_restart,
        "no hop-1 frame may be dropped across the restart while the reader keeps up"
    );

    // Unload path: the same guard handshake, then deactivate on the owner.
    let exit = rt_guard.take().unwrap().stop_and_join().unwrap();
    assert_eq!(exit.period_frames, 128, "the respawn continued at the reconciled block");
    instance.deactivate(exit.stopped);
    assert_eq!(s.stops.load(Relaxed), 2);
    assert_eq!(s.deactivations.load(Relaxed), 2);
    assert!(
        !s.contract_violation.load(Relaxed),
        "activate/deactivate on the owner thread only, start/stop/process on the RT thread only, and never process while inactive"
    );
    let owner = std::thread::current().id();
    assert!(
        s.main_thread_calls.lock().unwrap().iter().all(|t| *t == owner),
        "every activate/deactivate happened on the owner thread"
    );
    assert_eq!(diag.rt_faults.load(Relaxed), 0, "no RT fault was latched across the cycle");
    drop(ring);
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
    use super::super::engine_slot::{self, EngineSlotEvent, EngineSlotHandle, PluginFormat};
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
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink_seen = seen.clone();
        let obs = obs.clone();
        let handle = engine_slot::spawn(
            PluginFormat::Clap,
            device.host().slot(slot),
            0,
            Arc::new(move |e| sink_seen.lock().unwrap().push(e)),
            move |ctx| {
                super::super::clap_engine::run(ctx, "bleeploop.restart-fixture", move || {
                    OBSERVE.with(|o| *o.borrow_mut() = Some(obs));
                    // SAFETY: as `fixture_instance`: a static entry whose callbacks obey the ABI.
                    unsafe { PluginEntry::load_from_raw(&ENTRY, c"bleeploop-restart-fixture.clap") }
                        .map_err(|e| e.to_string())
                })
            },
        )
        .expect("the fixture loads into the engine");
        (handle, seen)
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
        assert!(wait_for(2000, || obs.processes.load(Relaxed) > 8), "the engine processes the unit");
        assert!(wait_for(2000, || rendered_above(&device, 0.2)), "its output reaches the device");
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

    #[test]
    fn an_effect_slot_takes_the_live_input_and_its_wet_output_reaches_the_device() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let obs = Arc::new(Observed::default());
        obs.inputs.store(2, Relaxed);
        let (handle, _) = load(&device, 0, &obs);
        assert_eq!(handle.kind(), SlotKind::Effect, "an audio input: an effect");
        assert!(wait_for(2000, || obs.processes.load(Relaxed) > 8 && rendered_near(&device, 0.0)), "not live: silence");
        assert_eq!(installed(&device, 0), Some((SlotKind::Effect, 0)));
        send(&device, Command::SetSlotLive(0, true));
        assert!(
            wait_for(2000, || rendered_near(&device, 0.1)),
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
        assert!(wait_for(2000, || obs.processes.load(Relaxed) > 8));
        obs.latency.store(128, Relaxed);
        let blocks_before = device.blocks.load(Relaxed);
        let processed_before = obs.processes.load(Relaxed);

        // Through the real host ABI, from a thread of the plugin's own.
        let host = unsafe { *obs.host.load(Relaxed) };
        std::thread::spawn(move || unsafe { (host.request_restart.unwrap())(&host) }).join().unwrap();
        assert!(
            wait_for(2000, || obs.activations.load(Relaxed) == 2
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
        assert!(wait_for(2000, || obs.processes.load(Relaxed) > 4));
        send(&device, Command::SelectInstrument(NoteTarget::Slot(1)));
        send(&device, Command::NoteOn(60, 0.5));
        assert!(wait_for(2000, || obs.note_ons.load(Relaxed) == 1), "the note-on reaches the plugin");
        assert_eq!(obs.last_key.load(Relaxed), 60);
        assert_eq!(f64::from_bits(obs.last_velocity.load(Relaxed)), 0.5);
        send(&device, Command::NoteOff(60));
        assert!(wait_for(2000, || obs.note_offs.load(Relaxed) == 1), "and its note-off");
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
        assert!(wait_for(2000, || obs.param_values.load(Relaxed) == 1), "the param reaches the processor");
        assert_eq!(obs.last_param.load(Relaxed), 101);
        assert_eq!(f64::from_bits(obs.last_param_value.load(Relaxed)), 0.75);
        handle.unload().unwrap();
    }

    #[test]
    fn an_eviction_reactivates_the_plugin_at_the_new_rate() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let obs = Arc::new(Observed::default());
        let (handle, _) = load(&device, 0, &obs);
        assert!(wait_for(2000, || obs.processes.load(Relaxed) > 4));

        device.rebuild_at(44_100, 1024);
        assert!(
            wait_for(2000, || obs.activations.load(Relaxed) == 2 && obs.starts.load(Relaxed) == 2),
            "the owner took the unit back and reinstalled it"
        );
        assert_eq!(f64::from_bits(obs.activated_rate.load(Relaxed)), 44_100.0, "re-activated at the new rate");
        assert_eq!(obs.activated_max_frames.load(Relaxed), 1024, "and the new engine's block");
        assert_eq!((obs.stops.load(Relaxed), obs.deactivations.load(Relaxed)), (1, 1));
        let processed = obs.processes.load(Relaxed);
        assert!(wait_for(2000, || obs.processes.load(Relaxed) > processed + 4), "the new engine processes it");
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
    use lf_engine::{SlotEvent, SlotEventKind, SlotProcessor};

    let mut instance = fixture_instance();
    let s = unsafe { state(instance.raw_instance() as *const clap_plugin) };
    s.output_level.store(0.5f32.to_bits(), Relaxed);
    assert!(
        super::engine_slot::rt_allocations(|| drop(std::hint::black_box(vec![0u8; 16]))) > 0,
        "the guard counts an allocation (control)"
    );
    let config = PluginAudioConfiguration { sample_rate: 48_000.0, min_frames_count: 1, max_frames_count: 64 };
    let stopped = instance.activate(|_, _| (), config).unwrap();
    let (mut params, ring) = RingBuffer::<PluginEvent>::new(8);
    let faults = Arc::new(AtomicU32::new(0));
    let terms = Terms { rate: 48_000, max_frames: 64, in_channels: 0, out_channels: 2, latency: 0 };
    let unit = ClapUnit::new(stopped, &terms, ring, faults.clone());
    let (mut unit, runs) = std::thread::spawn(move || {
        let mut unit = unit;
        let (input, mut out) = ([0.0f32; 200], [0.0f32; 200]);
        unit.process(0, &input[..64], &[], &mut out[..64]); // starts processing: one call
        params.push(PluginEvent::Param { id: 100, value: 0.5 }).unwrap();
        let notes = [
            SlotEvent { offset: 10, kind: SlotEventKind::NoteOn { key: 60, velocity: 1.0 } },
            SlotEvent { offset: 150, kind: SlotEventKind::NoteOn { key: 64, velocity: 1.0 } },
        ];
        // A window the shared counter was reset in runs again (`rt_allocations`); count the runs.
        let mut runs = 0;
        let allocations = super::engine_slot::rt_allocations(|| {
            runs += 1;
            unit.process(200, &input, &notes, &mut out)
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
