//! A raw CLAP plugin loaded by clack with the production LfHost. No audio device or GUI required.
use super::*;
use clap_sys::{
    entry::clap_plugin_entry,
    factory::plugin_factory::clap_plugin_factory,
    host::clap_host,
    plugin::{clap_plugin, clap_plugin_descriptor},
    version::CLAP_VERSION,
};
use std::ffi::{c_char, c_void, CStr};
use std::sync::atomic::AtomicUsize;
use std::thread::ThreadId;

const FEATURES: [*const c_char; 2] = [c"instrument".as_ptr(), std::ptr::null()];
static DESCRIPTOR: clap_plugin_descriptor = clap_plugin_descriptor {
    clap_version: CLAP_VERSION,
    id: c"bleeploop.callback-fixture".as_ptr(),
    name: c"Callback fixture".as_ptr(),
    vendor: c"BleepLoop".as_ptr(),
    url: c"".as_ptr(),
    manual_url: c"".as_ptr(),
    support_url: c"".as_ptr(),
    version: c"1".as_ptr(),
    description: c"".as_ptr(),
    features: FEATURES.as_ptr(),
};

struct FixtureState {
    host: *const clap_host,
    owner: ThreadId,
    calls: AtomicUsize,
    in_init: AtomicBool,
    invalid_delivery: AtomicBool,
}

unsafe fn state<'a>(plugin: *const clap_plugin) -> &'a FixtureState {
    // SAFETY: every fixture plugin owns this boxed state until its destroy callback.
    unsafe { &*((*plugin).plugin_data.cast::<FixtureState>()) }
}

unsafe extern "C" fn init(plugin: *const clap_plugin) -> bool {
    let s = unsafe { state(plugin) };
    s.in_init.store(true, Relaxed);
    for _ in 0..3 {
        unsafe { ((*s.host).request_callback.unwrap())(s.host) };
    }
    s.in_init.store(false, Relaxed);
    true
}

unsafe extern "C" fn on_main_thread(plugin: *const clap_plugin) {
    let s = unsafe { state(plugin) };
    if s.in_init.load(Relaxed) || s.owner != std::thread::current().id() {
        s.invalid_delivery.store(true, Relaxed);
    }
    let previous = s.calls.fetch_add(1, Relaxed);
    if previous == 0 {
        // Reentrant requests must coalesce into ONE future call, without recursive delivery.
        for _ in 0..2 {
            unsafe { ((*s.host).request_callback.unwrap())(s.host) };
        }
    }
}

unsafe extern "C" fn destroy(plugin: *const clap_plugin) {
    unsafe {
        drop(Box::from_raw((*plugin).plugin_data.cast::<FixtureState>()));
        drop(Box::from_raw(plugin.cast_mut()));
    }
}

unsafe extern "C" fn extension(_: *const clap_plugin, _: *const c_char) -> *const c_void {
    std::ptr::null()
}

unsafe extern "C" fn create(
    _: *const clap_plugin_factory,
    host: *const clap_host,
    id: *const c_char,
) -> *const clap_plugin {
    if unsafe { CStr::from_ptr(id) } != c"bleeploop.callback-fixture" {
        return std::ptr::null();
    }
    let state = Box::into_raw(Box::new(FixtureState {
        host,
        owner: std::thread::current().id(),
        calls: AtomicUsize::new(0),
        in_init: AtomicBool::new(false),
        invalid_delivery: AtomicBool::new(false),
    }));
    Box::into_raw(Box::new(clap_plugin {
        desc: &DESCRIPTOR,
        plugin_data: state.cast(),
        init: Some(init),
        destroy: Some(destroy),
        activate: None,
        deactivate: None,
        start_processing: None,
        stop_processing: None,
        reset: None,
        process: None,
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

#[test]
fn init_reentrant_and_background_requests_reach_the_owner_once_per_turn() {
    // SAFETY: the static entry and factory remain alive; clack owns each fixture instance, whose
    // init/main-thread/destroy callbacks obey the ABI. This test never activates its absent DSP.
    let entry =
        unsafe { PluginEntry::load_from_raw(&ENTRY, c"bleeploop-callback-fixture.clap") }.unwrap();
    let info = HostInfo::new("BleepLoop", "BleepLoop", "https://bleeploop.local", "0.1.0").unwrap();
    let mut instance = PluginInstance::<LfHost>::new(
        |_| LfShared {
            editor_closed: Arc::new(EditorClosed::default()),
            hosted_hwnd: Arc::new(AtomicIsize::new(0)),
            callback_requested: AtomicBool::new(false),
            restart_requested: AtomicBool::new(false),
        },
        |_| LfMain::default(),
        &entry,
        c"bleeploop.callback-fixture",
        &info,
    )
    .unwrap();
    let plugin = instance.raw_instance() as *const clap_plugin;
    let s = unsafe { state(plugin) };
    assert_eq!(
        s.calls.load(Relaxed),
        0,
        "must not call foreign code during init"
    );
    deliver_plugin_callback(&mut instance);
    assert_eq!(
        s.calls.load(Relaxed),
        1,
        "initialization request must reach the plugin"
    );
    deliver_plugin_callback(&mut instance);
    assert_eq!(
        s.calls.load(Relaxed),
        2,
        "reentrant requests survive to the next owner turn"
    );
    deliver_plugin_callback(&mut instance);
    assert_eq!(
        s.calls.load(Relaxed),
        2,
        "no request means no extra callback"
    );

    // clap_host is Send + Sync. Its copy retains clack's host_data; the instance outlives this thread.
    let host = unsafe { *s.host };
    std::thread::spawn(move || {
        #[cfg(debug_assertions)]
        let allocations_before = super::super::rt_alloc::RT_ALLOCS.load(Relaxed);
        {
            #[cfg(debug_assertions)]
            let _guard = super::super::rt_alloc::guard();
            for _ in 0..1000 {
                unsafe { (host.request_callback.unwrap())(&host) };
            }
        }
        #[cfg(debug_assertions)]
        assert_eq!(
            super::super::rt_alloc::RT_ALLOCS.load(Relaxed),
            allocations_before,
            "request_callback through the real host ABI must allocate nothing"
        );
    })
    .join()
    .unwrap();
    assert_eq!(
        s.calls.load(Relaxed),
        2,
        "background requests cannot invoke the owner callback inline"
    );
    deliver_plugin_callback(&mut instance);
    assert_eq!(s.calls.load(Relaxed), 3);
    deliver_plugin_callback(&mut instance);
    assert_eq!(s.calls.load(Relaxed), 3, "burst coalesces to one delivery");
    assert!(
        !s.invalid_delivery.load(Relaxed),
        "callback must run after init on the owner thread"
    );
}
