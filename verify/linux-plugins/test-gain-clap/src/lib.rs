//! LF Test Gain (CLAP): stereo in, stereo out, `out = in × GAIN`. One parameter, GAIN (id 7, so a
//! host that indexes instead of using ids is caught), 0..2, default 0.5; its state is that value as 8
//! little-endian bytes. Every sample it writes depends on its input, so a host check that compares a
//! render with and without it fails when the plugin is not in the path.

use std::ffi::{c_char, c_void, CStr};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};

use clap_sys::entry::clap_plugin_entry;
use clap_sys::events::{clap_event_header, clap_event_param_value, clap_input_events, clap_output_events, CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_PARAM_VALUE};
use clap_sys::ext::audio_ports::{clap_audio_port_info, clap_plugin_audio_ports, CLAP_AUDIO_PORT_IS_MAIN, CLAP_EXT_AUDIO_PORTS, CLAP_PORT_STEREO};
use clap_sys::ext::params::{clap_param_info, clap_plugin_params, CLAP_EXT_PARAMS, CLAP_PARAM_IS_AUTOMATABLE};
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

unsafe extern "C" fn init(_: *const clap_plugin) -> bool {
    true
}
unsafe extern "C" fn destroy(plugin: *const clap_plugin) {
    unsafe { drop(Box::from_raw((*plugin).plugin_data as *mut Gain)) }
}
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
    1
}
unsafe extern "C" fn params_info(_: *const clap_plugin, index: u32, info: *mut clap_param_info) -> bool {
    if index != 0 {
        return false;
    }
    unsafe {
        let info = &mut *info;
        info.id = GAIN_ID;
        info.flags = CLAP_PARAM_IS_AUTOMATABLE;
        info.cookie = ptr::null_mut();
        write_name(&mut info.name, c"Gain");
        write_name(&mut info.module, c"");
        info.min_value = 0.0;
        info.max_value = 2.0;
        info.default_value = GAIN_DEFAULT;
    }
    true
}
unsafe extern "C" fn params_value(plugin: *const clap_plugin, id: clap_id, value: *mut f64) -> bool {
    if id != GAIN_ID {
        return false;
    }
    unsafe { *value = Gain::of(plugin).gain() };
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
unsafe extern "C" fn factory_create(_: *const clap_plugin_factory, _host: *const clap_host, id: *const c_char) -> *const clap_plugin {
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
    }));
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
