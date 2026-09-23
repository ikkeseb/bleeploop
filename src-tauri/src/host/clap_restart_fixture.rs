//! A raw CLAP plugin with real activate/process callbacks, loaded by clack with the production
//! `LfHost` and driven by the production RT producer (`producer_loop`) against a plain heap ring in
//! place of the WebView2 SharedBuffer. Proves the plugin-initiated paths: `host.request_restart`
//! (deactivate → activate → RT resumed, on the right threads) and `clap_host_params.rescan`; and,
//! through its `clap.params` extension, that a malformed parameter count is refused.
//! No audio device or GUI required.
use super::super::transport::HOP1_HEADER_BYTES;
use super::*;
use clap_sys::{
    entry::clap_plugin_entry,
    ext::params::{
        clap_host_params, clap_param_info, clap_plugin_params, CLAP_EXT_PARAMS,
        CLAP_PARAM_RESCAN_VALUES,
    },
    factory::plugin_factory::clap_plugin_factory,
    host::clap_host,
    id::clap_id,
    plugin::{clap_plugin, clap_plugin_descriptor},
    process::{clap_process, clap_process_status, CLAP_PROCESS_CONTINUE},
    string_sizes::{CLAP_NAME_SIZE, CLAP_PATH_SIZE},
    version::CLAP_VERSION,
};
use std::ffi::{c_char, c_void, CStr};
use std::sync::atomic::AtomicUsize;
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

/// Every lifecycle call the plugin sees, with the thread it saw it on. `Mutex<Vec<ThreadId>>` on
/// the main-thread calls only (activate/deactivate, never on the RT path); the RT-side calls
/// (start/stop/process) record through atomics so the fixture allocates nothing under the
/// production RT alloc guard.
struct FixtureState {
    host: *const clap_host,
    owner: ThreadId,
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
}

unsafe fn state<'a>(plugin: *const clap_plugin) -> &'a FixtureState {
    // SAFETY: every fixture plugin owns this boxed state until its destroy callback.
    unsafe { &*((*plugin).plugin_data.cast::<FixtureState>()) }
}

unsafe extern "C" fn init(_: *const clap_plugin) -> bool {
    true
}

unsafe extern "C" fn activate(plugin: *const clap_plugin, _: f64, _: u32, _: u32) -> bool {
    let s = unsafe { state(plugin) };
    if s.owner != std::thread::current().id() || s.active.swap(true, Relaxed) {
        s.contract_violation.store(true, Relaxed);
    }
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
    s.starts.fetch_add(1, Relaxed);
    true
}

unsafe extern "C" fn stop_processing(plugin: *const clap_plugin) {
    let s = unsafe { state(plugin) };
    if s.owner == std::thread::current().id() || !s.processing.swap(false, Relaxed) {
        s.contract_violation.store(true, Relaxed);
    }
    s.stops.fetch_add(1, Relaxed);
}

unsafe extern "C" fn process(
    plugin: *const clap_plugin,
    _: *const clap_process,
) -> clap_process_status {
    let s = unsafe { state(plugin) };
    if s.owner == std::thread::current().id()
        || !s.active.load(Relaxed)
        || !s.processing.load(Relaxed)
    {
        s.contract_violation.store(true, Relaxed);
    }
    s.processes.fetch_add(1, Relaxed);
    CLAP_PROCESS_CONTINUE
}

unsafe extern "C" fn reset(_: *const clap_plugin) {}

unsafe extern "C" fn on_main_thread(_: *const clap_plugin) {}

unsafe extern "C" fn destroy(plugin: *const clap_plugin) {
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

static PARAMS: clap_plugin_params = clap_plugin_params {
    count: Some(param_count),
    get_info: Some(param_info),
    get_value: Some(param_value),
    value_to_text: None,
    text_to_value: None,
    flush: None,
};

unsafe extern "C" fn extension(_: *const clap_plugin, id: *const c_char) -> *const c_void {
    if unsafe { CStr::from_ptr(id) } == CLAP_EXT_PARAMS {
        (&PARAMS as *const clap_plugin_params).cast()
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
    let state = Box::into_raw(Box::new(FixtureState {
        host,
        owner: std::thread::current().id(),
        activations: AtomicUsize::new(0),
        deactivations: AtomicUsize::new(0),
        starts: AtomicUsize::new(0),
        stops: AtomicUsize::new(0),
        processes: AtomicUsize::new(0),
        active: AtomicBool::new(false),
        processing: AtomicBool::new(false),
        contract_violation: AtomicBool::new(false),
        main_thread_calls: Mutex::new(Vec::new()),
        param_count: AtomicU32::new(0),
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

    // Stand in for the JS drain: keep the header read cursor ([1]) caught up with the write cursor
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
