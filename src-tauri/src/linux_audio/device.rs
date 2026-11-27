//! One JACK client owns a duplex graph cycle. A candidate runs silence until its routes and actual
//! quantum are known; only then does the host hand it the engine from the preceding run.
#[path = "../engine_io/fpu.rs"]
mod fpu;
#[cfg(test)]
mod endpoint_test;
mod jack;
mod pipewire;
#[cfg(test)]
mod private_test;

use std::cell::Cell;
use std::collections::{BTreeMap, HashSet};
use std::ffi::{c_int, c_uint, c_void, CString};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicU8, Ordering::*};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use jack::{check, Client, Port, RawClient, INPUT, OUTPUT, PHYSICAL};
use lf_engine::{Engine, ProcessContext, SLOT_COUNT};
use rtrb::{Consumer, Producer, RingBuffer};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum InputPick {
    Mono(u32),
    Pair([u32; 2]),
}
impl InputPick {
    pub fn sides(self) -> [u32; 2] {
        match self {
            Self::Mono(c) => [c, c],
            Self::Pair(c) => c,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", from = "RequestWire")]
pub struct DeviceRequest {
    pub backend: String,
    pub input: Option<String>,
    pub output: Option<String>,
    pub input_channels: [Option<InputPick>; SLOT_COUNT],
    pub buffer: Option<u32>,
    pub sample_rate: Option<u32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RequestWire {
    backend: String,
    input: Option<String>,
    output: Option<String>,
    input_channels: Option<[Option<InputPick>; SLOT_COUNT]>,
    input_channel: Option<u32>,
    buffer: Option<u32>,
    sample_rate: Option<u32>,
}

impl From<RequestWire> for DeviceRequest {
    fn from(w: RequestWire) -> Self {
        Self {
            backend: w.backend,
            input: w.input,
            output: w.output,
            input_channels: w
                .input_channels
                .unwrap_or([w.input_channel.map(InputPick::Mono); SLOT_COUNT]),
            buffer: w.buffer,
            sample_rate: w.sample_rate,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceStatus {
    pub backend: String,
    pub sample_rate: u32,
    pub block: u32,
    pub input_name: String,
    pub output_name: String,
    pub input_open: bool,
    pub input_channels: [InputPick; SLOT_COUNT],
    pub input_requests: [Option<InputPick>; SLOT_COUNT],
    pub align_frames: i64,
    pub input_frames: i64,
    #[serde(default)]
    pub xruns: u64,
    #[serde(default)]
    pub missing_frames: u64,
}

#[derive(Debug, Serialize)]
pub struct Endpoint {
    pub id: String,
    pub name: String,
    pub channels: u32,
}

struct Group {
    info: Endpoint,
    ports: Vec<CString>,
    physical: bool,
}

// A capture endpoint is a source's `capture_*` ports, hardware or virtual, and a playback endpoint a
// sink's `playback_*` ports. pipewire-jack names an app's streams `output_*`/`input_*` and a sink's
// monitor `monitor_*`: those are never endpoints. Named JACK clients and their port names survive
// enumeration changes; server-assigned numeric IDs do not.
fn groups(client: &Client, capture: bool) -> Vec<Group> {
    let direction = if capture { OUTPUT } else { INPUT };
    let physical: HashSet<_> = client.names(direction | PHYSICAL).into_iter().collect();
    group_ports(client.names(direction), &physical, capture)
}

fn group_ports(ports: Vec<CString>, physical: &HashSet<CString>, capture: bool) -> Vec<Group> {
    let prefix = if capture { "capture_" } else { "playback_" };
    let mut groups = BTreeMap::new();
    for port in ports {
        let Ok(full_name) = port.to_str() else {
            continue;
        };
        let Some((owner, channel)) = full_name.split_once(':') else {
            continue;
        };
        if owner.starts_with("BleepLoop") || !channel.starts_with(prefix) {
            continue;
        }
        let group = groups.entry(owner.to_owned()).or_insert_with(|| Group {
            info: Endpoint {
                id: owner.to_owned(),
                name: owner.to_owned(),
                channels: 0,
            },
            ports: Vec::new(),
            physical: false,
        });
        group.physical |= physical.contains(&port);
        group.info.channels += 1;
        group.ports.push(port);
    }
    let mut result: Vec<_> = groups.into_values().collect();
    for group in &mut result {
        // Keep numeric channels in numeric order and the front stereo pair first on surround ports.
        group.ports.sort_by_cached_key(|name| {
            let name = name.to_string_lossy();
            let ch = name.split_once(':').map_or(name.as_ref(), |(_, ch)| ch);
            let suffix = ch.rsplit(['_', '-']).next().unwrap_or(ch);
            let order = match suffix {
                "FL" => 0,
                "FR" => 1,
                _ => suffix.parse::<u32>().unwrap_or(u32::MAX),
            };
            (order, ch.to_owned())
        });
    }
    result.sort_by_key(|group| !group.physical);
    result
}

/// An explicit id resolves only among the listed groups, so a listing and an open agree. Without
/// one, the first listed group that is not a webcam (physical ones come first); `avoid` runs only
/// then, since only that pick needs it.
fn selected(
    groups: Vec<Group>,
    id: &Option<String>,
    side: &str,
    avoid: impl FnOnce() -> HashSet<String>,
) -> Result<Option<Group>, String> {
    if let Some(id) = id {
        return groups
            .into_iter()
            .find(|g| g.info.id == *id)
            .map(Some)
            .ok_or_else(|| format!("JACK {side} endpoint {id:?} is unavailable"));
    }
    if groups.is_empty() {
        return Ok(None);
    }
    let avoid = avoid();
    Ok(groups.into_iter().find(|g| !avoid.contains(&g.info.id)))
}

fn endpoints(capture: bool) -> Result<Vec<Endpoint>, String> {
    let client = Client::open(c"BleepLoop-enumerate")?;
    Ok(groups(&client, capture)
        .into_iter()
        .map(|g| g.info)
        .collect())
}
pub fn list_inputs() -> Result<Vec<Endpoint>, String> {
    endpoints(true)
}
pub fn list_outputs() -> Result<Vec<Endpoint>, String> {
    endpoints(false)
}

fn channels(count: usize, asks: [Option<InputPick>; SLOT_COUNT]) -> [InputPick; SLOT_COUNT] {
    let auto = InputPick::Mono(u32::from(count >= 2));
    asks.map(|ask| {
        ask.filter(|p| p.sides().iter().all(|&n| (n as usize) < count))
            .unwrap_or(auto)
    })
}

const RT_CHUNK: usize = 512;
const MAX_PORTS: usize = 256;
type EngineAt = (Box<Engine>, i64);
struct Signals {
    input: Vec<*mut Port>,
    output: Vec<*mut Port>,
    // Four u8 channel indices, swapped together between blocks.
    picks: AtomicU32,
    peak: [[AtomicU32; 2]; SLOT_COUNT],
    capture_latency: AtomicU32,
    playback_latency: AtomicU32,
    rate: AtomicU32,
    block: AtomicU32,
    xrun: AtomicBool,
    xruns: AtomicU64,
    missing_frames: AtomicU64,
    fault: AtomicU8,
    graph_changed: AtomicBool,
    epoch: Instant,
    periods: AtomicU64,
    latency_dirty: AtomicBool,
    stamp_seq: AtomicU64,
    stamp_frame: AtomicI64,
    stamp_us: AtomicU64,
}
impl Signals {
    fn picks(&self, picks: [InputPick; SLOT_COUNT]) {
        let [a, b] = picks.map(InputPick::sides);
        self.picks.store(
            u32::from_ne_bytes([a[0] as u8, a[1] as u8, b[0] as u8, b[1] as u8]),
            Release,
        );
    }
    fn fail(&self, code: u8) {
        let _ = self.fault.compare_exchange(0, code, Release, Relaxed);
    }
    fn failure(&self) -> Option<&'static str> {
        match self.fault.load(Acquire) {
            0 => None,
            1 => Some("The JACK server stopped"),
            2 => Some("The JACK sample rate changed; reopen the device"),
            3 => Some("JACK supplied an invalid audio buffer"),
            4 => Some("A selected JACK port was disconnected"),
            _ => Some("The audio engine failed during a JACK callback"),
        }
    }
}

// The callback state is held through a raw owner pointer while JACK can borrow it. A Box must not
// create a competing &mut to a live callback. Closing the client joins JACK before reclaiming it.
struct Callback {
    client: *mut RawClient,
    signals: *const Signals,
    incoming: Consumer<EngineAt>,
    engine: Option<EngineAt>,
    expected_device: Option<u32>,
    capture_last_period: bool,
    panic: Option<Box<dyn std::any::Any + Send>>,
    left: [f32; RT_CHUNK],
    right: [f32; RT_CHUNK],
}
struct Run {
    client: Option<Client>,
    callback: NonNull<Callback>,
    signals: Arc<Signals>,
    sender: Producer<EngineAt>,
    links: Vec<(*mut Port, CString)>,
    input_id: Option<String>,
    output_id: String,
}
// JACK permits client operations from different control threads. Run has one non-RT owner under
// the host mutex; process alone touches Callback until client_close has joined all callbacks.
unsafe impl Send for Run {}

impl Run {
    fn close(&mut self) {
        drop(self.client.take());
    }
    fn refresh_latencies(&self) {
        self.signals.latency_dirty.store(false, Release);
        if self.signals.failure().is_some() {
            return;
        }
        // Notify the graph off the audio thread. Some backends deliver the resulting callback
        // asynchronously; query the connected ranges now, and that callback will publish its update.
        unsafe {
            jack::jack_recompute_total_latencies(self.client.as_ref().unwrap().raw());
        }
        for mode in [0, 1] {
            read_latency(&self.signals, mode);
        }
    }
    fn routes_connected(&self) -> bool {
        self.links
            .iter()
            .all(|(ours, peer)| unsafe { jack::jack_port_connected_to(*ours, peer.as_ptr()) != 0 })
    }
}
impl Drop for Run {
    fn drop(&mut self) {
        self.close();
        unsafe {
            drop(Box::from_raw(self.callback.as_ptr()));
        }
    }
}

pub struct Prepared {
    requested_quantum: Option<u32>,
    run: Run,
    status: DeviceStatus,
}
pub struct Device {
    run: Run,
    status: DeviceStatus,
}

impl Prepared {
    pub fn new(request: &DeviceRequest) -> Result<Self, String> {
        if request.backend != "Jack" {
            return Err(format!(
                "Linux audio requires JACK, got {:?}",
                request.backend
            ));
        }
        if request.buffer == Some(0) {
            return Err("A JACK buffer must contain at least one frame".into());
        }
        let client = Client::open(c"BleepLoop")?;
        let raw = client.raw();
        // A webcam's mic is listed but never the default input; with no other source the device
        // opens output-only.
        let input = selected(
            groups(&client, true),
            &request.input,
            "capture",
            pipewire::webcams,
        )?;
        let output = selected(
            groups(&client, false),
            &request.output,
            "playback",
            HashSet::new,
        )?
        .ok_or(
            "No JACK playback endpoint found. Select an available output in Audio Settings.",
        )?;
        let input_id = input.as_ref().map(|g| g.info.id.clone());
        let output_id = output.info.id.clone();
        let input_name = input
            .as_ref()
            .map_or_else(String::new, |g| g.info.name.clone());
        let mut links = Vec::new();
        let mut inputs = Vec::new();
        let mut outputs = Vec::new();
        for peer in input.into_iter().flat_map(|g| g.ports) {
            if inputs.len() == MAX_PORTS {
                return Err(format!("JACK input exceeds {MAX_PORTS} channels"));
            }
            let name = CString::new(format!("in_{}", inputs.len() + 1)).unwrap();
            let ours = client.register(&name, INPUT)?;
            inputs.push(ours);
            links.push((ours, peer));
        }
        for (name, peer) in [c"out_L", c"out_R"].into_iter().zip(output.ports) {
            let ours = client.register(name, OUTPUT)?;
            outputs.push(ours);
            links.push((ours, peer));
        }
        let input_count = inputs.len();
        let picks = channels(input_count, request.input_channels);
        let signals = Arc::new(Signals {
            input: inputs,
            output: outputs,
            picks: 0.into(),
            peak: Default::default(),
            capture_latency: 0.into(),
            playback_latency: 0.into(),
            rate: unsafe { jack::jack_get_sample_rate(raw) }.into(),
            block: 0.into(),
            xrun: false.into(),
            xruns: 0.into(),
            missing_frames: 0.into(),
            fault: 0.into(),
            graph_changed: false.into(),
            epoch: Instant::now(),
            periods: 0.into(),
            latency_dirty: true.into(),
            stamp_seq: 0.into(),
            stamp_frame: 0.into(),
            stamp_us: 0.into(),
        });
        signals.picks(picks);
        let (sender, incoming) = RingBuffer::new(1);
        let shared = Arc::as_ptr(&signals).cast_mut().cast::<c_void>();
        let callback = NonNull::from(Box::leak(Box::new(Callback {
            client: raw,
            signals: Arc::as_ptr(&signals),
            incoming,
            engine: None,
            expected_device: None,
            capture_last_period: false,
            panic: None,
            left: [0.0; RT_CHUNK],
            right: [0.0; RT_CHUNK],
        })));
        let run = Run {
            client: Some(client),
            callback,
            signals,
            sender,
            links,
            input_id,
            output_id,
        };
        unsafe {
            check(
                "JACK process callback",
                jack::jack_set_process_callback(raw, process, callback.as_ptr().cast()),
            )?;
            check(
                "JACK xrun callback",
                jack::jack_set_xrun_callback(raw, xrun, shared),
            )?;
            check(
                "JACK rate callback",
                jack::jack_set_sample_rate_callback(raw, changed_rate, shared),
            )?;
            check(
                "JACK latency callback",
                jack::jack_set_latency_callback(raw, latency, shared),
            )?;
            check(
                "JACK graph callback",
                jack::jack_set_port_connect_callback(raw, changed_graph, shared),
            )?;
            jack::jack_on_shutdown(raw, shutdown, shared);
            check("Activating JACK", jack::jack_activate(raw))?;
        }
        let client = run.client.as_ref().unwrap();
        for (index, (ours, peer)) in run.links.iter().enumerate() {
            client.link(*ours, peer, index < input_count)?;
        }
        wait_ready(
            &run,
            || run.signals.block.load(Acquire) > 0 && run.routes_connected(),
            "JACK did not start the requested audio routes",
        )?;
        let rate = run.signals.rate.load(Acquire);
        if !(8000..=192000).contains(&rate) {
            return Err(format!("Unsupported JACK sample rate: {rate} Hz"));
        }
        if request.sample_rate.is_some_and(|r| r != rate) {
            return Err(format!("JACK is running at {rate} Hz. Change the server rate or choose the server default."));
        }
        let status = DeviceStatus {
            backend: "Jack".into(),
            sample_rate: rate,
            block: run.signals.block.load(Acquire),
            input_name,
            output_name: output.info.name,
            input_open: input_count > 0,
            input_channels: picks,
            input_requests: request.input_channels,
            align_frames: 0,
            input_frames: 0,
            xruns: 0,
            missing_frames: 0,
        };
        Ok(Self {
            run,
            status,
            requested_quantum: None,
        })
    }

    /// Called only after the host has accepted a possible sample-rate change. PipeWire can pin
    /// this client node to the chosen quantum even if a different node currently supplies it.
    pub fn request_buffer(&mut self, buffer: Option<u32>) -> Result<(), String> {
        if let Some(buffer) = buffer {
            check("Changing the JACK graph buffer", unsafe {
                jack::jack_set_buffer_size(self.run.client.as_ref().unwrap().raw(), buffer)
            })?;
        }
        self.requested_quantum = buffer;
        Ok(())
    }
    /// The previous client is gone. Read callbacks from the graph now running, including when
    /// releasing a smaller PipeWire node request allows the graph to resume its own period.
    pub fn settle(&self) -> Result<(), String> {
        let s = &self.run.signals;
        let since = s.periods.load(Acquire);
        wait_ready(
            &self.run,
            || {
                s.periods.load(Acquire).wrapping_sub(since) >= 3
                    && self
                        .requested_quantum
                        .is_none_or(|q| q == s.block.load(Acquire))
            },
            &format!(
                "JACK did not settle at the requested buffer ({:?})",
                self.requested_quantum
            ),
        )?;
        self.run.refresh_latencies();
        Ok(())
    }

    pub fn rate(&self) -> u32 {
        self.status.sample_rate
    }
    pub fn start(
        mut self,
        engine: Box<Engine>,
        first_frame: i64,
    ) -> Result<Device, (String, Box<Engine>)> {
        if let Some(why) = self.run.signals.failure() {
            return Err((why.into(), engine));
        }
        if engine.config().sample_rate != self.rate() || engine.config().max_block == 0 {
            return Err((
                "Engine and JACK device configurations disagree".into(),
                engine,
            ));
        }
        if !self.run.routes_connected() {
            return Err((
                "A JACK route disappeared during device switching".into(),
                engine,
            ));
        }
        self.run.sender.push((engine, first_frame)).map_err(
            |rtrb::PushError::Full((engine, _))| {
                ("The JACK client already owns an engine".into(), engine)
            },
        )?;
        Ok(Device {
            run: self.run,
            status: self.status,
        })
    }
}

fn wait_ready(run: &Run, ready: impl Fn() -> bool, timeout: &str) -> Result<(), String> {
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(why) = run.signals.failure() {
            return Err(why.into());
        }
        if ready() {
            return Ok(());
        }
        if Instant::now() >= until {
            return Err(timeout.into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

impl Device {
    /// Pinning an already-selected default to its explicit endpoint is only a preference change.
    /// Going back to a default still resolves the graph again; releasing our buffer request must
    /// retire the client that owns it.
    pub fn can_keep(&self, old: &DeviceRequest, new: &DeviceRequest) -> bool {
        old.backend == new.backend
            && old.input_channels == new.input_channels
            && old.sample_rate == new.sample_rate
            && (old.input == new.input || new.input.is_some() && new.input == self.run.input_id)
            && (old.output == new.output || new.output.as_ref() == Some(&self.run.output_id))
            && old.buffer == new.buffer
            && new
                .buffer
                .is_none_or(|q| q == self.run.signals.block.load(Acquire))
    }
    pub fn stop(mut self) -> (Box<Engine>, i64, bool) {
        self.run.close();
        // No callbacks can reach this allocation after close.
        let cb = unsafe { self.run.callback.as_mut() };
        let (engine, frame) = cb
            .engine
            .take()
            .or_else(|| cb.incoming.pop().ok())
            .expect("a started device owns its engine");
        (engine, frame, cb.panic.is_some())
    }
    pub fn status(&self) -> DeviceStatus {
        let s = &self.run.signals;
        if s.latency_dirty.load(Acquire) {
            self.run.refresh_latencies();
        }
        let mut status = self.status.clone();
        status.block = s.block.load(Acquire);
        status.xruns = s.xruns.load(Relaxed);
        status.missing_frames = s.missing_frames.load(Relaxed);
        status.input_frames = i64::from(s.capture_latency.load(Relaxed));
        status.align_frames = status.input_frames + i64::from(s.playback_latency.load(Relaxed));
        status
    }
    pub fn fault(&self) -> Option<&'static str> {
        let s = &self.run.signals;
        if s.failure().is_none()
            && s.graph_changed.swap(false, AcqRel)
            && !self.run.routes_connected()
        {
            s.fail(4);
        }
        s.failure()
    }
    pub fn set_channel(&mut self, slot: usize, request: Option<InputPick>) -> Result<(), String> {
        if slot >= SLOT_COUNT {
            return Err(format!("Invalid JACK input slot {slot}"));
        }
        let count = self.run.signals.input.len();
        if let Some(n) = request
            .into_iter()
            .flat_map(|p| p.sides())
            .find(|&n| n as usize >= count)
        {
            return Err(format!(
                "Input channel {} is unavailable ({count} channels)",
                u64::from(n) + 1
            ));
        }
        self.status.input_requests[slot] = request;
        self.status.input_channels = channels(count, self.status.input_requests);
        self.run.signals.picks(self.status.input_channels);
        Ok(())
    }
    pub fn meter(&self) -> ([[f32; 2]; SLOT_COUNT], [bool; SLOT_COUNT]) {
        let peak = self
            .run
            .signals
            .peak
            .each_ref()
            .map(|p| p.each_ref().map(|v| f32::from_bits(v.swap(0, Relaxed))));
        (peak, peak.map(|p| p[0] >= 1.0 || p[1] >= 1.0))
    }
    pub fn anchor(&self) -> Option<(i64, f64)> {
        let s = &self.run.signals;
        loop {
            let before = s.stamp_seq.load(SeqCst);
            if before == 0 {
                return None;
            }
            let frame = s.stamp_frame.load(SeqCst);
            let us = s.stamp_us.load(SeqCst);
            if before % 2 == 0 && before == s.stamp_seq.load(SeqCst) {
                let elapsed = s.epoch.elapsed().saturating_sub(Duration::from_micros(us));
                let at = SystemTime::now()
                    .checked_sub(elapsed)?
                    .duration_since(UNIX_EPOCH)
                    .ok()?;
                return Some((frame, at.as_secs_f64() * 1000.));
            }
        }
    }
}

// All notification callbacks use only the immutable Signals address; JACK can run them alongside
// process. Its latency callback propagates the undelayed monitor path to the opposite port set.
extern "C" fn shutdown(arg: *mut c_void) {
    unsafe { &*arg.cast::<Signals>() }.fail(1);
}
extern "C" fn xrun(arg: *mut c_void) -> c_int {
    let signals = unsafe { &*arg.cast::<Signals>() };
    signals.xruns.fetch_add(1, Relaxed);
    signals.xrun.store(true, Release);
    0
}
extern "C" fn changed_graph(_: u32, _: u32, _: c_int, arg: *mut c_void) {
    let s = unsafe { &*arg.cast::<Signals>() };
    s.graph_changed.store(true, Release);
    s.latency_dirty.store(true, Release);
}
extern "C" fn changed_rate(rate: u32, arg: *mut c_void) -> c_int {
    let s = unsafe { &*arg.cast::<Signals>() };
    let old = s.rate.swap(rate, AcqRel);
    if s.block.load(Acquire) > 0 && old != rate {
        s.fail(2);
    }
    0
}
extern "C" fn latency(mode: c_uint, arg: *mut c_void) {
    let s = unsafe { &*arg.cast::<Signals>() };
    let mut range = read_latency(s, mode);
    let to = if mode == 0 { &s.output } else { &s.input };
    for &port in to {
        unsafe { jack::jack_port_set_latency_range(port, mode, &mut range) };
    }
}
fn read_latency(s: &Signals, mode: c_uint) -> jack::Latency {
    let (from, store) = if mode == 0 {
        (&s.input, &s.capture_latency)
    } else {
        (&s.output, &s.playback_latency)
    };
    let mut range = jack::Latency {
        min: u32::MAX,
        max: 0,
    };
    for &port in from {
        let mut r = jack::Latency::default();
        unsafe { jack::jack_port_get_latency_range(port, mode, &mut r) };
        range.min = range.min.min(r.min);
        range.max = range.max.max(r.max);
    }
    if from.is_empty() {
        range.min = 0;
    }
    store.store(range.max, Relaxed);
    range
}

// The app panic hook skips formatting and logging while the JACK thread owns the engine.
thread_local! { static IN_CALLBACK: Cell<bool> = const { Cell::new(false) }; }

pub(crate) fn in_rt_callback() -> bool {
    IN_CALLBACK.get()
}

struct RtSection;
impl Drop for RtSection {
    fn drop(&mut self) {
        IN_CALLBACK.set(false);
    }
}

fn period_gap(
    clock_gap: bool,
    graph_report: bool,
    has_capture: bool,
    previous_capture: bool,
) -> (bool, lf_engine::engine::CaptureActivity) {
    let graph_capture = graph_report && has_capture;
    let missed = clock_gap || (graph_capture && previous_capture);
    (
        missed,
        lf_engine::engine::CaptureActivity::new(graph_capture && !missed),
    )
}

extern "C" fn process(frames: u32, arg: *mut c_void) -> c_int {
    IN_CALLBACK.set(true);
    let _rt = RtSection;
    // JACK serializes process for this client. The control thread joins it before touching Callback.
    let cb = unsafe { &mut *arg.cast::<Callback>() };
    let s = unsafe { &*cb.signals };
    let n = frames as usize;
    if n == 0 || n > isize::MAX as usize / size_of::<f32>() {
        s.fail(3);
        return -1;
    }
    let mut out = [std::ptr::null_mut::<f32>(); 2];
    for (buf, &port) in out.iter_mut().zip(&s.output) {
        *buf = unsafe { jack::jack_port_get_buffer(port, frames) }.cast();
        if buf.is_null() || !buf.is_aligned() {
            s.fail(3);
            return -1;
        }
    }
    let stereo = s.output.len() == 2;
    if stereo && out[0].addr().abs_diff(out[1].addr()) < n * size_of::<f32>() {
        s.fail(3);
        return -1;
    }
    if s.block.swap(frames, AcqRel) != frames {
        s.latency_dirty.store(true, Release);
    }
    s.periods.fetch_add(1, Release);
    if cb.engine.is_none() {
        cb.engine = cb.incoming.pop().ok();
    }
    let Some((engine, next_frame)) = cb.engine.as_mut().filter(|_| s.failure().is_none()) else {
        for buf in out.into_iter().take(s.output.len()) {
            unsafe { buf.write_bytes(0, n) };
        }
        return 0;
    };
    let _mode = fpu::DenormalsOff::new();
    let device_frame = unsafe { jack::jack_last_frame_time(cb.client) };
    let gap = cb
        .expected_device
        .map_or(0, |expected| device_frame.wrapping_sub(expected));
    // A backward jump is a discontinuity, not four billion missing frames. Keep engine time monotonic.
    let frame = *next_frame
        + if gap <= i32::MAX as u32 {
            i64::from(gap)
        } else {
            0
        };
    if gap > 0 && gap <= i32::MAX as u32 {
        s.missing_frames.fetch_add(u64::from(gap), Relaxed);
    }
    cb.expected_device = Some(device_frame.wrapping_add(frames));
    // PipeWire's JACK notification follows the graph driver's xrun counter, even when this
    // client delivered every frame. Keep the graph diagnostic independently. A break in our own
    // clock always damages a take. A graph report can damage external input on either side of
    // this boundary, including a live gate opening or finishing its ramp within either period.
    let (missed, mut activity) = period_gap(
        gap != 0,
        s.xrun.swap(false, AcqRel),
        !s.input.is_empty(),
        cb.capture_last_period,
    );
    s.stamp_seq.fetch_add(1, SeqCst);
    s.stamp_frame.store(frame, SeqCst);
    s.stamp_us
        .store(s.epoch.elapsed().as_micros() as u64, SeqCst);
    s.stamp_seq.fetch_add(1, SeqCst);
    let mut input = [std::ptr::null::<f32>(); 4];
    for (buf, pick) in input.iter_mut().zip(s.picks.load(Acquire).to_ne_bytes()) {
        if let Some(&port) = s.input.get(usize::from(pick)) {
            *buf = unsafe { jack::jack_port_get_buffer(port, frames) }.cast();
            // Identical input/output buffers work in-place one chunk at a time. A partial
            // overlap could overwrite a later chunk before the engine reads it.
            if buf.is_null()
                || !buf.is_aligned()
                || out[..s.output.len()].iter().any(|&dst| {
                    let offset = dst.addr().abs_diff(buf.addr());
                    offset != 0 && offset < n * size_of::<f32>()
                })
            {
                s.fail(3);
                return -1;
            }
        }
    }
    let in_latency = i64::from(s.capture_latency.load(Relaxed));
    let align_frames = in_latency + i64::from(s.playback_latency.load(Relaxed));
    let step = RT_CHUNK.min(engine.config().max_block);
    let silence = [0.0; RT_CHUNK];
    let mut peaks = [0.0f32; 4];
    // Render into private scratch before writing JACK output. JACK may reuse an input buffer for
    // output, or two input ports may share a buffer. No JACK output is borrowed as a Rust reference.
    let rendered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        for start in (0..n).step_by(step) {
            let len = step.min(n - start);
            let samples = input.map(|ptr| {
                if ptr.is_null() {
                    &silence[..len]
                } else {
                    unsafe { std::slice::from_raw_parts(ptr.add(start), len) }
                }
            });
            for (peak, samples) in peaks.iter_mut().zip(samples) {
                *peak = samples.iter().fold(*peak, |p, &x| p.max(x.abs()));
            }
            let ctx = ProcessContext {
                frame: frame + start as i64,
                xrun: missed && start == 0,
                damaged: false,
                input_frames: in_latency,
                align_frames,
            };
            engine.process_inputs_observed(
                &ctx,
                [[samples[0], samples[1]], [samples[2], samples[3]]],
                &mut cb.left[..len],
                &mut cb.right[..len],
                &mut activity,
            );
            // A mono sink gets a centre-preserving fold; JACK audio samples are normalized floats.
            for i in 0..len {
                let limit = |x: f32| {
                    if x.is_finite() {
                        x.clamp(-1.0, 1.0)
                    } else {
                        0.0
                    }
                };
                unsafe {
                    if stereo {
                        out[0].add(start + i).write(limit(cb.left[i]));
                        out[1].add(start + i).write(limit(cb.right[i]));
                    } else {
                        out[0]
                            .add(start + i)
                            .write(limit(0.5 * (cb.left[i] + cb.right[i])));
                    }
                }
            }
        }
    }));
    if let Err(payload) = rendered {
        cb.panic = Some(payload);
        for buf in out.into_iter().take(s.output.len()) {
            unsafe { buf.write_bytes(0, n) };
        }
        s.fail(5);
        return 0;
    }
    for (value, peak) in s.peak.iter().flatten().zip(peaks) {
        value.fetch_max(peak.min(f32::MAX).to_bits(), Relaxed);
    }
    cb.capture_last_period = activity.routed;
    *next_frame = frame + n as i64;
    0
}
