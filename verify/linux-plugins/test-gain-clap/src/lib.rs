//! LF Test Gain (CLAP): stereo in, stereo out, `out = in × GAIN`. One parameter, GAIN (id 7, so a
//! host that indexes instead of using ids is caught), 0..2, default 0.5; its state is that value as 8
//! little-endian bytes. Every sample it writes depends on its input, so a host check that compares a
//! render with and without it fails when the plugin is not in the path.
//!
//! It also drives the host's run loop with no editor open: at `init` it registers a 10 ms timer
//! (`clap.timer-support`) and the read end of a socket pair (`clap.posix-fd-support`); each tick
//! counts and writes a byte to the pair, each fd callback reads it and counts. The counts are two
//! more parameters, TICKS (id 8) and FD WAKES (id 9): not flagged read-only (the host lists no such
//! parameter), and a value the host sets for them is ignored. `destroy` unregisters both and frees the
//! plugin; a run-loop callback for an instance that is gone, or for a timer or fd this instance does
//! not hold, aborts the process.
//!
//! One misbehaviour on purpose: a stored state of the 8 bytes `LEAKREGS` is refused, and the instance
//! that refused it leaves its timer and fd registered at `destroy` (its socket pair stays open, a byte
//! pending): what a host must forget itself, since the next instance shares its run loop.

use std::ffi::{c_char, c_void, CStr};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

/// The instances alive (their addresses): a run-loop callback for any other aborts.
static LIVE: Mutex<Vec<usize>> = Mutex::new(Vec::new());
/// The state that is refused and makes its instance leak its registrations.
const LEAK_STATE: &[u8; 8] = b"LEAKREGS";

use clap_sys::entry::clap_plugin_entry;
use clap_sys::events::{clap_event_header, clap_event_param_value, clap_input_events, clap_output_events, CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_PARAM_VALUE};
use clap_sys::ext::audio_ports::{clap_audio_port_info, clap_plugin_audio_ports, CLAP_AUDIO_PORT_IS_MAIN, CLAP_EXT_AUDIO_PORTS, CLAP_PORT_STEREO};
use clap_sys::ext::params::{clap_param_info, clap_plugin_params, CLAP_EXT_PARAMS, CLAP_PARAM_IS_AUTOMATABLE};
use clap_sys::ext::posix_fd_support::{clap_host_posix_fd_support, clap_plugin_posix_fd_support, clap_posix_fd_flags, CLAP_EXT_POSIX_FD_SUPPORT, CLAP_POSIX_FD_READ};
use clap_sys::ext::timer_support::{clap_host_timer_support, clap_plugin_timer_support, CLAP_EXT_TIMER_SUPPORT};
use clap_sys::ext::state::{clap_plugin_state, CLAP_EXT_STATE};
use clap_sys::factory::plugin_factory::{clap_plugin_factory, CLAP_PLUGIN_FACTORY_ID};
use clap_sys::host::clap_host;
use clap_sys::id::clap_id;
use clap_sys::plugin::{clap_plugin, clap_plugin_descriptor};
use clap_sys::plugin_features::{CLAP_PLUGIN_FEATURE_AUDIO_EFFECT, CLAP_PLUGIN_FEATURE_STEREO};
use clap_sys::process::{clap_process, clap_process_status, CLAP_PROCESS_CONTINUE, CLAP_PROCESS_ERROR};
use clap_sys::stream::{clap_istream, clap_ostream};
use clap_sys::version::CLAP_VERSION;

const PLUGIN_ID: &CStr = c"dev.bleeploop.test-gain";
const GAIN_ID: clap_id = 7;
const GAIN_DEFAULT: f64 = 0.5;
const TICKS_ID: clap_id = 8;
const FD_WAKES_ID: clap_id = 9;
const TIMER_MS: u32 = 10;
const NO_TIMER: u32 = u32::MAX;

struct Features([*const c_char; 3]);
unsafe impl Sync for Features {}
static FEATURES: Features = Features([CLAP_PLUGIN_FEATURE_AUDIO_EFFECT.as_ptr(), CLAP_PLUGIN_FEATURE_STEREO.as_ptr(), ptr::null()]);

static DESCRIPTOR: clap_plugin_descriptor = clap_plugin_descriptor {
    clap_version: CLAP_VERSION,
    id: PLUGIN_ID.as_ptr(),
    name: c"LF Test Gain".as_ptr(),
    vendor: c"BleepLoop".as_ptr(),
    url: c"".as_ptr(),
    manual_url: c"".as_ptr(),
    support_url: c"".as_ptr(),
    version: c"0.1.0".as_ptr(),
    description: c"Test plugin: out = in x GAIN".as_ptr(),
    features: &FEATURES.0 as *const _ as *const *const c_char,
};

/// The plugin's data: the clap_plugin first, so its pointer is the struct's.
#[repr(C)]
struct Gain {
    plugin: clap_plugin,
    /// GAIN as f64 bits: the main thread (params flush, state) and the audio thread both touch it.
    gain: AtomicU64,
    host: *const clap_host,
    /// The registered timer's id, `NO_TIMER` for none.
    timer: AtomicU32,
    /// The socket pair whose read end is registered: (read, write).
    pair: Mutex<Option<(UnixStream, UnixStream)>>,
    ticks: AtomicU64,
    fd_wakes: AtomicU64,
    /// It refused `LEAK_STATE`: `destroy` leaves its timer and fd registered.
    leak: AtomicBool,
}

impl Gain {
    unsafe fn of<'a>(plugin: *const clap_plugin) -> &'a Gain {
        unsafe { &*((*plugin).plugin_data as *const Gain) }
    }
    fn gain(&self) -> f64 {
        f64::from_bits(self.gain.load(Ordering::Relaxed))
    }
    fn set_gain(&self, value: f64) {
        self.gain.store(value.clamp(0.0, 2.0).to_bits(), Ordering::Relaxed);
    }
    unsafe fn apply_events(&self, events: *const clap_input_events) {
        if events.is_null() {
            return;
        }
        unsafe {
            let (Some(size), Some(get)) = ((*events).size, (*events).get) else { return };
            for i in 0..size(events) {
                let header: *const clap_event_header = get(events, i);
                if header.is_null() || (*header).space_id != CLAP_CORE_EVENT_SPACE_ID || (*header).type_ != CLAP_EVENT_PARAM_VALUE {
                    continue;
                }
                let event = &*(header as *const clap_event_param_value);
                if event.param_id == GAIN_ID {
                    self.set_gain(event.value);
                }
            }
        }
    }
}

/// The host's extension `id`, if it has it.
unsafe fn host_ext<T>(host: *const clap_host, id: &CStr) -> Option<&'static T> {
    unsafe {
        let ext = ((*host).get_extension?)(host, id.as_ptr()) as *const T;
        ext.as_ref()
    }
}

unsafe extern "C" fn init(plugin: *const clap_plugin) -> bool {
    unsafe {
        let me = Gain::of(plugin);
        if let Some(timers) = host_ext::<clap_host_timer_support>(me.host, CLAP_EXT_TIMER_SUPPORT) {
            let mut id = NO_TIMER;
            if timers.register_timer.is_some_and(|register| register(me.host, TIMER_MS, &mut id)) {
                me.timer.store(id, Ordering::Relaxed);
            }
        }
        if let Some(fds) = host_ext::<clap_host_posix_fd_support>(me.host, CLAP_EXT_POSIX_FD_SUPPORT) {
            if let Ok((read, write)) = UnixStream::pair() {
                let _ = read.set_nonblocking(true);
                let _ = write.set_nonblocking(true);
                if fds.register_fd.is_some_and(|register| register(me.host, read.as_raw_fd(), CLAP_POSIX_FD_READ)) {
                    *me.pair.lock().unwrap() = Some((read, write));
                }
            }
        }
    }
    true
}
unsafe extern "C" fn destroy(plugin: *const clap_plugin) {
    unsafe {
        let me = Gain::of(plugin);
        LIVE.lock().unwrap().retain(|&p| p != plugin as usize);
        let pair = me.pair.lock().unwrap().take();
        if me.leak.load(Ordering::Relaxed) {
            // Left registered, the read end readable: only the host's own forgetting keeps either
            // from calling the instance that comes next.
            if let Some((read, mut write)) = pair {
                let _ = write.write(&[1]);
                std::mem::forget((read, write));
            }
        } else {
            let timer = me.timer.swap(NO_TIMER, Ordering::Relaxed);
            if timer != NO_TIMER {
                if let Some(unregister) = host_ext::<clap_host_timer_support>(me.host, CLAP_EXT_TIMER_SUPPORT).and_then(|t| t.unregister_timer) {
                    unregister(me.host, timer);
                }
            }
            if let Some((read, _)) = &pair {
                if let Some(unregister) = host_ext::<clap_host_posix_fd_support>(me.host, CLAP_EXT_POSIX_FD_SUPPORT).and_then(|f| f.unregister_fd) {
                    unregister(me.host, read.as_raw_fd());
                }
            }
            drop(pair);
        }
        drop(Box::from_raw(plugin as *mut Gain));
    }
}

/// A run-loop callback's instance, or the process ends: a callback for an instance that is gone
/// (after its `destroy`) or for a registration it does not hold (`own`).
unsafe fn alive<'a>(plugin: *const clap_plugin, what: &str, own: impl FnOnce(&Gain) -> bool) -> &'a Gain {
    if !LIVE.lock().unwrap().contains(&(plugin as usize)) {
        eprintln!("LF Test Gain: {what} after destroy");
        std::process::abort();
    }
    let me = unsafe { Gain::of(plugin) };
    if !own(me) {
        eprintln!("LF Test Gain: {what} for a registration this instance does not hold");
        std::process::abort();
    }
    me
}
unsafe extern "C" fn on_timer(plugin: *const clap_plugin, timer_id: clap_id) {
    let me = unsafe { alive(plugin, "on_timer", |me| me.timer.load(Ordering::Relaxed) == timer_id) };
    me.ticks.fetch_add(1, Ordering::Relaxed);
    if let Some((_, write)) = me.pair.lock().unwrap().as_mut() {
        let _ = write.write(&[1]);
    }
}
static TIMER: clap_plugin_timer_support = clap_plugin_timer_support { on_timer: Some(on_timer) };
unsafe extern "C" fn on_fd(plugin: *const clap_plugin, fd: i32, flags: clap_posix_fd_flags) {
    let me = unsafe { alive(plugin, "on_fd", |me| me.pair.lock().unwrap().as_ref().is_some_and(|(r, _)| r.as_raw_fd() == fd)) };
    let mut pair = me.pair.lock().unwrap();
    let Some((read, _)) = pair.as_mut() else { return };
    if flags & CLAP_POSIX_FD_READ != 0 {
        let mut buf = [0u8; 64];
        while read.read(&mut buf).is_ok_and(|n| n > 0) {}
        me.fd_wakes.fetch_add(1, Ordering::Relaxed);
    }
}
static FD: clap_plugin_posix_fd_support = clap_plugin_posix_fd_support { on_fd: Some(on_fd) };
unsafe extern "C" fn activate(_: *const clap_plugin, _: f64, _: u32, _: u32) -> bool {
    true
}
unsafe extern "C" fn deactivate(_: *const clap_plugin) {}
unsafe extern "C" fn start_processing(_: *const clap_plugin) -> bool {
    true
}
unsafe extern "C" fn stop_processing(_: *const clap_plugin) {}
unsafe extern "C" fn reset(_: *const clap_plugin) {}
unsafe extern "C" fn on_main_thread(_: *const clap_plugin) {}

unsafe extern "C" fn process(plugin: *const clap_plugin, process: *const clap_process) -> clap_process_status {
    unsafe {
        let me = Gain::of(plugin);
        let p = &*process;
        me.apply_events(p.in_events);
        if p.audio_inputs_count < 1 || p.audio_outputs_count < 1 {
            return CLAP_PROCESS_ERROR;
        }
        let input = &*p.audio_inputs;
        let output = &*p.audio_outputs;
        let gain = me.gain() as f32;
        let frames = p.frames_count as usize;
        for ch in 0..output.channel_count.min(2) as usize {
            let src = *input.data32.add(ch.min(input.channel_count as usize - 1));
            let dst = *output.data32.add(ch);
            for i in 0..frames {
                *dst.add(i) = *src.add(i) * gain;
            }
        }
        CLAP_PROCESS_CONTINUE
    }
}

unsafe extern "C" fn ports_count(_: *const clap_plugin, _: bool) -> u32 {
    1
}
unsafe extern "C" fn ports_get(_: *const clap_plugin, index: u32, is_input: bool, info: *mut clap_audio_port_info) -> bool {
    if index != 0 {
        return false;
    }
    unsafe {
        let info = &mut *info;
        info.id = if is_input { 0 } else { 1 };
        write_name(&mut info.name, if is_input { c"in" } else { c"out" });
        info.flags = CLAP_AUDIO_PORT_IS_MAIN;
        info.channel_count = 2;
        info.port_type = CLAP_PORT_STEREO.as_ptr();
        info.in_place_pair = clap_sys::id::CLAP_INVALID_ID;
    }
    true
}
static AUDIO_PORTS: clap_plugin_audio_ports = clap_plugin_audio_ports { count: Some(ports_count), get: Some(ports_get) };

unsafe extern "C" fn params_count(_: *const clap_plugin) -> u32 {
    3
}
unsafe extern "C" fn params_info(_: *const clap_plugin, index: u32, info: *mut clap_param_info) -> bool {
    let (id, name, flags, max, default) = match index {
        0 => (GAIN_ID, c"Gain", CLAP_PARAM_IS_AUTOMATABLE, 2.0, GAIN_DEFAULT),
        1 => (TICKS_ID, c"Ticks", 0, 1e9, 0.0),
        2 => (FD_WAKES_ID, c"Fd wakes", 0, 1e9, 0.0),
        _ => return false,
    };
    unsafe {
        let info = &mut *info;
        info.id = id;
        info.flags = flags;
        info.cookie = ptr::null_mut();
        write_name(&mut info.name, name);
        write_name(&mut info.module, c"");
        info.min_value = 0.0;
        info.max_value = max;
        info.default_value = default;
    }
    true
}
unsafe extern "C" fn params_value(plugin: *const clap_plugin, id: clap_id, value: *mut f64) -> bool {
    let me = unsafe { Gain::of(plugin) };
    let got = match id {
        GAIN_ID => me.gain(),
        TICKS_ID => me.ticks.load(Ordering::Relaxed) as f64,
        FD_WAKES_ID => me.fd_wakes.load(Ordering::Relaxed) as f64,
        _ => return false,
    };
    unsafe { *value = got };
    true
}
unsafe extern "C" fn params_to_text(_: *const clap_plugin, _: clap_id, value: f64, out: *mut c_char, size: u32) -> bool {
    let text = format!("{value:.3}\0");
    if text.len() > size as usize {
        return false;
    }
    unsafe { ptr::copy_nonoverlapping(text.as_ptr() as *const c_char, out, text.len()) };
    true
}
unsafe extern "C" fn params_from_text(_: *const clap_plugin, _: clap_id, text: *const c_char, value: *mut f64) -> bool {
    let Ok(parsed) = unsafe { CStr::from_ptr(text) }.to_string_lossy().trim().parse::<f64>() else { return false };
    unsafe { *value = parsed };
    true
}
unsafe extern "C" fn params_flush(plugin: *const clap_plugin, events: *const clap_input_events, _: *const clap_output_events) {
    unsafe { Gain::of(plugin).apply_events(events) }
}
static PARAMS: clap_plugin_params = clap_plugin_params {
    count: Some(params_count),
    get_info: Some(params_info),
    get_value: Some(params_value),
    value_to_text: Some(params_to_text),
    text_to_value: Some(params_from_text),
    flush: Some(params_flush),
};

unsafe extern "C" fn state_save(plugin: *const clap_plugin, stream: *const clap_ostream) -> bool {
    unsafe {
        let bytes = Gain::of(plugin).gain().to_le_bytes();
        let Some(write) = (*stream).write else { return false };
        write(stream, bytes.as_ptr() as *const c_void, bytes.len() as u64) == bytes.len() as i64
    }
}
unsafe extern "C" fn state_load(plugin: *const clap_plugin, stream: *const clap_istream) -> bool {
    unsafe {
        let mut bytes = [0u8; 8];
        let Some(read) = (*stream).read else { return false };
        let mut got = 0usize;
        while got < 8 {
            let n = read(stream, bytes.as_mut_ptr().add(got) as *mut c_void, (8 - got) as u64);
            if n <= 0 {
                return false;
            }
            got += n as usize;
        }
        if &bytes == LEAK_STATE {
            Gain::of(plugin).leak.store(true, Ordering::Relaxed);
            return false;
        }
        Gain::of(plugin).set_gain(f64::from_le_bytes(bytes));
        true
    }
}
static STATE: clap_plugin_state = clap_plugin_state { save: Some(state_save), load: Some(state_load) };

unsafe extern "C" fn get_extension(_: *const clap_plugin, id: *const c_char) -> *const c_void {
    let id = unsafe { CStr::from_ptr(id) };
    if id == CLAP_EXT_AUDIO_PORTS {
        &AUDIO_PORTS as *const _ as *const c_void
    } else if id == CLAP_EXT_PARAMS {
        &PARAMS as *const _ as *const c_void
    } else if id == CLAP_EXT_STATE {
        &STATE as *const _ as *const c_void
    } else if id == CLAP_EXT_TIMER_SUPPORT {
        &TIMER as *const _ as *const c_void
    } else if id == CLAP_EXT_POSIX_FD_SUPPORT {
        &FD as *const _ as *const c_void
    } else {
        ptr::null()
    }
}

fn write_name<const N: usize>(dst: &mut [c_char; N], src: &CStr) {
    let bytes = src.to_bytes();
    let n = bytes.len().min(N - 1);
    for (d, s) in dst.iter_mut().zip(&bytes[..n]) {
        *d = *s as c_char;
    }
    dst[n] = 0;
}

unsafe extern "C" fn factory_count(_: *const clap_plugin_factory) -> u32 {
    1
}
unsafe extern "C" fn factory_descriptor(_: *const clap_plugin_factory, index: u32) -> *const clap_plugin_descriptor {
    if index == 0 { &DESCRIPTOR } else { ptr::null() }
}
unsafe extern "C" fn factory_create(_: *const clap_plugin_factory, host: *const clap_host, id: *const c_char) -> *const clap_plugin {
    if unsafe { CStr::from_ptr(id) } != PLUGIN_ID {
        return ptr::null();
    }
    let gain = Box::into_raw(Box::new(Gain {
        plugin: clap_plugin {
            desc: &DESCRIPTOR,
            plugin_data: ptr::null_mut(),
            init: Some(init),
            destroy: Some(destroy),
            activate: Some(activate),
            deactivate: Some(deactivate),
            start_processing: Some(start_processing),
            stop_processing: Some(stop_processing),
            reset: Some(reset),
            process: Some(process),
            get_extension: Some(get_extension),
            on_main_thread: Some(on_main_thread),
        },
        gain: AtomicU64::new(GAIN_DEFAULT.to_bits()),
        host,
        timer: AtomicU32::new(NO_TIMER),
        pair: Mutex::new(None),
        ticks: AtomicU64::new(0),
        fd_wakes: AtomicU64::new(0),
        leak: AtomicBool::new(false),
    }));
    LIVE.lock().unwrap().push(gain as usize);
    unsafe {
        (*gain).plugin.plugin_data = gain as *mut c_void;
        &(*gain).plugin
    }
}
static FACTORY: clap_plugin_factory = clap_plugin_factory {
    get_plugin_count: Some(factory_count),
    get_plugin_descriptor: Some(factory_descriptor),
    create_plugin: Some(factory_create),
};

unsafe extern "C" fn entry_init(_: *const c_char) -> bool {
    true
}
unsafe extern "C" fn entry_deinit() {}
unsafe extern "C" fn entry_factory(id: *const c_char) -> *const c_void {
    if unsafe { CStr::from_ptr(id) } == CLAP_PLUGIN_FACTORY_ID { &FACTORY as *const _ as *const c_void } else { ptr::null() }
}

#[no_mangle]
#[used]
pub static clap_entry: clap_plugin_entry = clap_plugin_entry {
    clap_version: CLAP_VERSION,
    init: Some(entry_init),
    deinit: Some(entry_deinit),
    get_factory: Some(entry_factory),
};
