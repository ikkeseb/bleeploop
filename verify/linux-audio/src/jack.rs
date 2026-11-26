//! The JACK C API used by this probe, linked to the system libjack.
use std::ffi::{c_char, c_int, c_uint, c_ulong, c_void, CStr, CString};
use std::ptr::NonNull;
use std::slice;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;

const NO_START_SERVER: c_uint = 1;
pub const INPUT: c_ulong = 1;
pub const OUTPUT: c_ulong = 2;
pub const PHYSICAL: c_ulong = 4;
const AUDIO: &CStr = c"32 bit float mono audio";

enum RawClient {}
enum RawPort {}
type ProcessFn = extern "C" fn(u32, *mut c_void) -> c_int;
type NotifyFn = extern "C" fn(*mut c_void) -> c_int;
type FreewheelFn = extern "C" fn(c_int, *mut c_void);

#[link(name = "jack")]
unsafe extern "C" {
    fn jack_client_open(
        name: *const c_char,
        options: c_uint,
        status: *mut c_uint,
        ...
    ) -> *mut RawClient;
    fn jack_client_close(client: *mut RawClient) -> c_int;
    fn jack_activate(client: *mut RawClient) -> c_int;
    fn jack_get_sample_rate(client: *mut RawClient) -> u32;
    fn jack_get_buffer_size(client: *mut RawClient) -> u32;
    fn jack_last_frame_time(client: *mut RawClient) -> u32;
    fn jack_port_register(
        client: *mut RawClient,
        name: *const c_char,
        kind: *const c_char,
        flags: c_ulong,
        buffer_size: c_ulong,
    ) -> *mut RawPort;
    fn jack_port_by_name(client: *mut RawClient, name: *const c_char) -> *mut RawPort;
    fn jack_port_uuid(port: *const RawPort) -> u64;
    fn jack_port_flags(port: *const RawPort) -> c_int;
    fn jack_port_type(port: *const RawPort) -> *const c_char;
    fn jack_port_name(port: *const RawPort) -> *const c_char;
    fn jack_port_get_buffer(port: *mut RawPort, frames: u32) -> *mut c_void;
    fn jack_port_connected(port: *const RawPort) -> c_int;
    fn jack_port_connected_to(port: *const RawPort, name: *const c_char) -> c_int;
    fn jack_connect(client: *mut RawClient, source: *const c_char, dest: *const c_char) -> c_int;
    fn jack_set_process_callback(
        client: *mut RawClient,
        callback: ProcessFn,
        state: *mut c_void,
    ) -> c_int;
    fn jack_set_xrun_callback(
        client: *mut RawClient,
        callback: NotifyFn,
        state: *mut c_void,
    ) -> c_int;
    fn jack_set_sample_rate_callback(
        client: *mut RawClient,
        callback: ProcessFn,
        state: *mut c_void,
    ) -> c_int;
    fn jack_set_freewheel_callback(
        client: *mut RawClient,
        callback: FreewheelFn,
        state: *mut c_void,
    ) -> c_int;
}

#[derive(Default)]
pub struct DeviceEvents {
    pub xruns: AtomicU32,
    pub sample_rate: AtomicU32,
    pub freewheel: AtomicBool,
    buffer_fault: AtomicU8,
}

const INVALID_BUFFER: u8 = 1;
const OVERLAPPING_BUFFERS: u8 = 2;

impl DeviceEvents {
    fn reject_buffer(&self, fault: u8) -> c_int {
        let _ = self
            .buffer_fault
            .compare_exchange(0, fault, Ordering::Relaxed, Ordering::Relaxed);
        -1
    }

    pub fn check_buffers(&self) -> Result<(), &'static str> {
        match self.buffer_fault.load(Ordering::Relaxed) {
            0 => Ok(()),
            OVERLAPPING_BUFFERS => Err("JACK callback rejected overlapping audio buffers"),
            _ => Err("JACK callback received a null audio buffer or an invalid buffer size"),
        }
    }
}

pub struct Endpoint {
    pub uuid: u64,
    pub name: CString,
    pub physical: bool,
}

pub struct Client {
    raw: NonNull<RawClient>,
    events: Arc<DeviceEvents>,
    callback: Option<NonNull<Callback>>,
}

type Process = dyn FnMut(u32, &[f32], &mut [f32], &mut [f32]) + Send;
struct Callback {
    client: NonNull<RawClient>,
    ports: [NonNull<RawPort>; 3],
    process: Box<Process>,
    events: Arc<DeviceEvents>,
}

fn check(operation: &str, code: c_int) -> Result<(), String> {
    if code == 0 {
        Ok(())
    } else {
        Err(format!("{operation} failed with JACK code {code}"))
    }
}

impl Client {
    pub fn open() -> Result<Self, String> {
        let mut status = 0;
        // SAFETY: These options take no trailing arguments. The name and status live for the call.
        let raw = unsafe {
            jack_client_open(
                c"bleeploop-linux-probe".as_ptr(),
                NO_START_SERVER,
                &mut status,
            )
        };
        let raw = NonNull::new(raw).ok_or_else(|| {
            format!("Could not open the existing JACK server (status {status:#x})")
        })?;
        Ok(Self {
            raw,
            events: Arc::new(DeviceEvents::default()),
            callback: None,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        // SAFETY: self owns an open JACK client.
        unsafe { jack_get_sample_rate(self.raw.as_ptr()) }
    }

    pub fn buffer_size(&self) -> u32 {
        // SAFETY: self owns an open JACK client.
        unsafe { jack_get_buffer_size(self.raw.as_ptr()) }
    }

    pub fn endpoint(&self, name: &str, direction: c_ulong) -> Result<Endpoint, String> {
        let name = CString::new(name).map_err(|_| "Port name contains a NUL byte")?;
        // SAFETY: Query JACK's port metadata and keep the caller-owned name for routing.
        unsafe {
            let port = jack_port_by_name(self.raw.as_ptr(), name.as_ptr());
            if port.is_null() {
                return Err(format!("JACK port {:?} was not found", name));
            }
            let flags = jack_port_flags(port) as c_ulong;
            let kind = jack_port_type(port);
            if flags & direction == 0 || kind.is_null() || CStr::from_ptr(kind) != AUDIO {
                return Err(format!(
                    "{name:?} is not a JACK audio port in the required direction"
                ));
            }
            Ok(Endpoint {
                name,
                uuid: jack_port_uuid(port),
                physical: flags & PHYSICAL != 0,
            })
        }
    }

    pub fn activate<F>(&mut self, process: F) -> Result<[CString; 3], String>
    where
        F: FnMut(u32, &[f32], &mut [f32], &mut [f32]) + Send + 'static,
    {
        if self.callback.is_some() {
            return Err("The JACK client is already registered for playback".into());
        }
        let raw = self.raw.as_ptr();
        // SAFETY: The client is inactive, and it owns the ports once registered. The boxed callback
        // and ref-counted event state live until client close has stopped all callbacks.
        unsafe {
            let register = |name: &CStr, direction| {
                NonNull::new(jack_port_register(
                    raw,
                    name.as_ptr(),
                    AUDIO.as_ptr(),
                    direction,
                    0,
                ))
                .ok_or_else(|| format!("Could not register JACK port {name:?}"))
            };
            let ports = [
                register(c"capture", INPUT)?,
                register(c"left", OUTPUT)?,
                register(c"right", OUTPUT)?,
            ];
            let name = |port: NonNull<RawPort>| {
                let ptr = NonNull::new(jack_port_name(port.as_ptr()).cast_mut())
                    .ok_or("Registered JACK port disappeared")?;
                Ok::<_, String>(CStr::from_ptr(ptr.as_ptr()).to_owned())
            };
            let names = [name(ports[0])?, name(ports[1])?, name(ports[2])?];
            self.events
                .sample_rate
                .store(self.sample_rate(), Ordering::Relaxed);
            let events = Arc::as_ptr(&self.events).cast_mut().cast();
            let data = NonNull::from(Box::leak(Box::new(Callback {
                client: self.raw,
                ports,
                process: Box::new(process),
                events: Arc::clone(&self.events),
            })));
            self.callback = Some(data);
            check(
                "jack_set_process_callback",
                jack_set_process_callback(raw, process_audio, data.as_ptr().cast()),
            )?;
            check(
                "jack_set_xrun_callback",
                jack_set_xrun_callback(raw, notify_xrun, events),
            )?;
            check(
                "jack_set_sample_rate_callback",
                jack_set_sample_rate_callback(raw, notify_rate, events),
            )?;
            check(
                "jack_set_freewheel_callback",
                jack_set_freewheel_callback(raw, notify_freewheel, events),
            )?;
            check("jack_activate", jack_activate(raw))?;
            Ok(names)
        }
    }

    pub fn events(&self) -> &Arc<DeviceEvents> {
        &self.events
    }

    pub fn connect(&self, source: &CStr, dest: &CStr) -> Result<(), String> {
        // SAFETY: The client and both names are valid for the call.
        let code = unsafe { jack_connect(self.raw.as_ptr(), source.as_ptr(), dest.as_ptr()) };
        check(&format!("connect {source:?} → {dest:?}"), code)
    }

    pub fn exclusive_route(&self, our_port: &CStr, peer: &CStr) -> bool {
        // SAFETY: Only query port pointers returned by JACK for this live client. Our own ports
        // remain registered through close, and the peer is referenced by name.
        unsafe {
            let port = jack_port_by_name(self.raw.as_ptr(), our_port.as_ptr());
            !port.is_null()
                && jack_port_connected(port) == 1
                && jack_port_connected_to(port, peer.as_ptr()) != 0
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // SAFETY: This is the sole owner. Close stops the callbacks before their state is freed.
        unsafe {
            jack_client_close(self.raw.as_ptr());
            if let Some(callback) = self.callback.take() {
                drop(Box::from_raw(callback.as_ptr()));
            }
        }
    }
}

extern "C" fn process_audio(frames: u32, state: *mut c_void) -> c_int {
    // SAFETY: JACK serializes this client's process callbacks and supplies aligned audio buffers
    // for this cycle. A graph change can alias buffers, so check before borrowing them as slices.
    unsafe {
        let state = &mut *state.cast::<Callback>();
        let [input, left, right] = state
            .ports
            .map(|port| jack_port_get_buffer(port.as_ptr(), frames).cast::<f32>());
        let bytes = (frames as usize).saturating_mul(size_of::<f32>());
        if bytes > isize::MAX as usize || input.is_null() || left.is_null() || right.is_null() {
            return state.events.reject_buffer(INVALID_BUFFER);
        }
        if input.addr().abs_diff(left.addr()) < bytes
            || input.addr().abs_diff(right.addr()) < bytes
            || left.addr().abs_diff(right.addr()) < bytes
        {
            return state.events.reject_buffer(OVERLAPPING_BUFFERS);
        }
        // Output-only ports supply writable storage. Initialize it before borrowing it as f32.
        left.write_bytes(0, frames as usize);
        right.write_bytes(0, frames as usize);
        (state.process)(
            jack_last_frame_time(state.client.as_ptr()),
            slice::from_raw_parts(input, frames as usize),
            slice::from_raw_parts_mut(left, frames as usize),
            slice::from_raw_parts_mut(right, frames as usize),
        );
    }
    0
}

extern "C" fn notify_xrun(state: *mut c_void) -> c_int {
    // SAFETY: This pointer comes from the Arc kept by the client, with atomics for shared access.
    unsafe { &*state.cast::<DeviceEvents>() }
        .xruns
        .fetch_add(1, Ordering::Relaxed);
    0
}

extern "C" fn notify_rate(rate: u32, state: *mut c_void) -> c_int {
    // SAFETY: This pointer comes from the Arc kept by the client, with atomics for shared access.
    unsafe { &*state.cast::<DeviceEvents>() }
        .sample_rate
        .store(rate, Ordering::Relaxed);
    0
}

extern "C" fn notify_freewheel(starting: c_int, state: *mut c_void) {
    // SAFETY: This pointer comes from the Arc kept by the client, with atomics for shared access.
    if starting != 0 {
        unsafe { &*state.cast::<DeviceEvents>() }
            .freewheel
            .store(true, Ordering::Relaxed);
    }
}
