//! JACK's C ABI. A Client owns its registrations and links until close has joined its callbacks.
use std::ffi::{c_char, c_int, c_uint, c_ulong, c_void, CStr, CString};
use std::ptr::NonNull;

pub(super) enum RawClient {}
pub(super) enum Port {}
pub(super) const INPUT: c_ulong = 1;
pub(super) const OUTPUT: c_ulong = 2;
pub(super) const PHYSICAL: c_ulong = 4;
const AUDIO: &CStr = c"32 bit float mono audio";

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub(super) struct Latency {
    pub min: u32,
    pub max: u32,
}

#[link(name = "jack")]
unsafe extern "C" {
    fn jack_client_open(
        name: *const c_char,
        options: c_uint,
        status: *mut c_uint,
        ...
    ) -> *mut RawClient;
    fn jack_client_close(client: *mut RawClient) -> c_int;
    fn jack_get_ports(
        client: *mut RawClient,
        pattern: *const c_char,
        kind: *const c_char,
        flags: c_ulong,
    ) -> *mut *const c_char;
    fn jack_free(p: *mut c_void);
    fn jack_port_register(
        client: *mut RawClient,
        name: *const c_char,
        kind: *const c_char,
        flags: c_ulong,
        size: c_ulong,
    ) -> *mut Port;
    fn jack_port_name(port: *const Port) -> *const c_char;
    fn jack_connect(client: *mut RawClient, source: *const c_char, dest: *const c_char) -> c_int;
    pub fn jack_recompute_total_latencies(client: *mut RawClient) -> c_int;
    pub fn jack_get_sample_rate(client: *mut RawClient) -> u32;
    pub fn jack_set_buffer_size(client: *mut RawClient, frames: u32) -> c_int;
    pub fn jack_last_frame_time(client: *mut RawClient) -> u32;
    pub fn jack_port_get_buffer(port: *mut Port, frames: u32) -> *mut c_void;
    pub fn jack_port_connected_to(port: *const Port, peer: *const c_char) -> c_int;
    pub fn jack_set_process_callback(
        client: *mut RawClient,
        cb: extern "C" fn(u32, *mut c_void) -> c_int,
        arg: *mut c_void,
    ) -> c_int;
    pub fn jack_set_xrun_callback(
        client: *mut RawClient,
        cb: extern "C" fn(*mut c_void) -> c_int,
        arg: *mut c_void,
    ) -> c_int;
    pub fn jack_set_sample_rate_callback(
        client: *mut RawClient,
        cb: extern "C" fn(u32, *mut c_void) -> c_int,
        arg: *mut c_void,
    ) -> c_int;
    pub fn jack_set_port_connect_callback(
        client: *mut RawClient,
        cb: extern "C" fn(u32, u32, c_int, *mut c_void),
        arg: *mut c_void,
    ) -> c_int;
    pub fn jack_set_latency_callback(
        client: *mut RawClient,
        cb: extern "C" fn(c_uint, *mut c_void),
        arg: *mut c_void,
    ) -> c_int;
    pub fn jack_port_get_latency_range(port: *mut Port, mode: c_uint, range: *mut Latency);
    pub fn jack_port_set_latency_range(port: *mut Port, mode: c_uint, range: *mut Latency);
    pub fn jack_on_shutdown(
        client: *mut RawClient,
        cb: extern "C" fn(*mut c_void),
        arg: *mut c_void,
    );
    pub fn jack_activate(client: *mut RawClient) -> c_int;
}

pub(super) fn check(operation: &str, code: c_int) -> Result<(), String> {
    if code == 0 {
        Ok(())
    } else {
        Err(format!("{operation} failed (JACK error {code})"))
    }
}

pub(super) struct Client(NonNull<RawClient>);
impl Client {
    pub fn open(name: &CStr) -> Result<Self, String> {
        let mut status = 0;
        // JackNoStartServer takes no trailing arguments.
        let raw = unsafe { jack_client_open(name.as_ptr(), 1, &mut status) };
        NonNull::new(raw).map(Self).ok_or_else(|| format!("Could not connect to JACK (status {status:#x}). Start JACK or run BleepLoop with PipeWire's pw-jack."))
    }

    pub fn raw(&self) -> *mut RawClient {
        self.0.as_ptr()
    }

    pub fn names(&self, flags: c_ulong) -> Vec<CString> {
        let mut names = Vec::new();
        // JACK owns the entries until the returned null-terminated list is freed.
        unsafe {
            let list = jack_get_ports(self.raw(), std::ptr::null(), AUDIO.as_ptr(), flags);
            if !list.is_null() {
                let mut entry = list;
                while !(*entry).is_null() {
                    names.push(CStr::from_ptr(*entry).to_owned());
                    entry = entry.add(1);
                }
                jack_free(list.cast());
            }
        }
        names
    }

    pub fn register(&self, name: &CStr, flags: c_ulong) -> Result<*mut Port, String> {
        let raw =
            unsafe { jack_port_register(self.raw(), name.as_ptr(), AUDIO.as_ptr(), flags, 0) };
        NonNull::new(raw)
            .map(NonNull::as_ptr)
            .ok_or_else(|| format!("Could not register JACK port {name:?}"))
    }

    pub fn link(&self, port: *mut Port, peer: &CStr, capture: bool) -> Result<(), String> {
        // Only our own registration is dereferenced; the peer can disappear at any moment.
        let name = unsafe { jack_port_name(port) };
        if name.is_null() {
            return Err("JACK port disappeared before connecting".into());
        }
        let ours = unsafe { CStr::from_ptr(name) };
        let (source, dest) = if capture { (peer, ours) } else { (ours, peer) };
        let code = unsafe { jack_connect(self.raw(), source.as_ptr(), dest.as_ptr()) };
        if code != 0 && unsafe { jack_port_connected_to(port, peer.as_ptr()) } == 0 {
            return Err(format!(
                "Could not connect {source:?} to {dest:?} (JACK error {code})"
            ));
        }
        Ok(())
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // The owning Device drops this before freeing its callback pointers.
        unsafe {
            jack_client_close(self.raw());
        }
    }
}
