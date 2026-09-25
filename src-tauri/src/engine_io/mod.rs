//! engine_io: the device side of the native engine (`docs/plans/native-engine.md` § Stage 4). Dormant
//! until the plan's Stage 6 flip: a DEV probe drives it, and nothing on the live line calls it. This doc
//! is the module's briefing.
//!
//! [`EngineHost`] is the process-wide handle: one device owner thread serializes every device
//! transition (open, backend switch, channel change, loss, close); the engine ([`lf_engine::Engine`])
//! sits in one `Mutex` that the device callback only `try_lock`s (a miss plays silence and counts) and
//! that the owner takes only with both streams dropped, so the engine and the plugin slots outlive
//! device switches and loss. The callback body runs under `catch_unwind` inside the lock guard: a caught
//! panic latches a fault and plays silence until a new engine replaces it, the `Mutex` is never
//! poisoned, and nothing unwinds into asio-sys's `extern "C"` bufferSwitch.
//!
//! # Module map
//!
//! | Module | Owns |
//! |---|---|
//! | this file | [`EngineHost`] and the state its threads share ([`Core`]) |
//! | `owner` | the device owner thread: every transition, one at a time, and the fallback on a loss |
//! | `transition` | the owner's decisions as pure functions (the kernel), with their table tests |
//! | `callback` | the callback bodies (ASIO and WASAPI, input and output) and a run's shared state |
//! | `driver` | the seam the owner opens streams through; `cpal_driver` is the real one, `fake_driver` (tests) the hardware-free one |
//! | `slot_host` | [`SlotHost`]: a plugin owner's install/remove/eviction handshake with the engine |
//! | `frame_clock` | [`FrameClock`]: the callback's (time, frame) stamp and a press's frame |
//! | `pipes` | [`pipes::PullPipe`]: frames pushed on one clock, pulled resampled on another (the WASAPI join, Share output) |
//! | `share` | Share output: the post-limiter master mirrored to a WASAPI endpoint while ASIO plays |
//! | `midi` | native MIDI: ports, hot-plug, parse, the MIDI-learn bindings, notes and pedal actions |
//!
//! # Rules
//!
//! - **One clock: the output callback's frame counter.** ASIO: input built first, output second,
//!   always as a pair (asio-sys 0.3.0 runs the registered callbacks in that order in one bufferSwitch;
//!   the output checks the input's cycle count: a miss is a duplex-order fault, counted). WASAPI: the
//!   output callback is the clock; the input joins through a ring and a resampler with a drift
//!   controller. Every callback thread is promoted to MMCSS Pro Audio on first entry.
//! - **The frame counter pauses across a switch.** A backend switch or a fallback continues the
//!   counter where the last callback left it, so loops resume in place; a device gap inside a run
//!   (an xrun) jumps it by the frames lost and flags `ProcessContext::xrun`.
//! - **Every stop fades and punches out.** A switch or close ramps the output to silence (10 ms, then
//!   two silent callbacks: a dropped ASIO stream leaves the driver playing its last two buffers), drops
//!   the output then the input, and punches out a take in flight (STATUS E3). A loss drops at once. A
//!   new sample rate builds a new engine: the plugin units go back to their owners (`SlotHost`), and the
//!   loops go with the old engine.
//! - **`Core::running` is up from just before the streams start until just after they drop,** so a
//!   slot host never takes the engine lock from under a callback (it waits on its port instead).
//! - **The callback never allocates, logs, locks (beyond its `try_lock`) or waits.** Counters are
//!   atomics the owner reads; the owner logs.
//! - **cpal is pinned at `=0.18.1`:** the callback order the Stage 1 A1 run proved is read from
//!   asio-sys 0.3.0; 0.18.2 moves to asio-sys 0.4.0 and windows 0.62 and needs its own A1 rerun.

mod callback;
mod cpal_driver;
mod driver;
#[cfg(test)]
mod fake_driver;
pub mod frame_clock;
pub mod midi;
mod owner;
pub(crate) mod pipes;
pub mod share;
pub mod slot_host;
#[cfg(test)]
pub(crate) mod test_rig;
#[cfg(test)]
mod tests;
mod transition;

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lf_engine::grid::Frame;
use lf_engine::{Engine, Event, SlotPort, SlotProcessor, TimedCommand, SLOT_COUNT};
use rtrb::{Consumer, Producer};

use crate::audio_output::AudioBackend;
use callback::{Tap, TapEnd, MAX_DEVICE_BLOCK};
use driver::Driver;
use owner::{OwnerLink, Request};

pub use frame_clock::FrameClock;
pub use slot_host::SlotHost;

/// What the host is built with.
#[derive(Clone, Copy, Debug)]
pub struct HostConfig {
    /// Lane buffer length (`lf_engine::EngineConfig::max_loop_seconds`).
    pub max_loop_seconds: f64,
}

impl Default for HostConfig {
    fn default() -> Self {
        HostConfig { max_loop_seconds: 60.0 }
    }
}

/// A device to open (or switch to).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceRequest {
    pub backend: AudioBackend,
    /// WASAPI capture device id (`audio_input::list_input_devices`); `None` = the default. ASIO uses
    /// the cached duplex driver (`audio_output::asio_cache`) and ignores both ids.
    pub input: Option<String>,
    /// WASAPI render device id (`audio_output::list_output_devices`); `None` = the default.
    pub output: Option<String>,
    /// The capture channel (0-based); `None` = auto (`audio_input::InputChannelControl`).
    pub input_channel: Option<u32>,
    /// Frames per device callback; `None` = the driver's default.
    pub buffer: Option<u32>,
}

/// The device that runs.
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceStatus {
    pub backend: AudioBackend,
    pub sample_rate: u32,
    /// Frames per output callback (the driver's; a WASAPI callback may vary around it).
    pub block: u32,
    pub input_name: String,
    pub output_name: String,
    /// Input plus output latency in frames (`ProcessContext::align_frames`), and its input side.
    pub align_frames: Frame,
    pub input_frames: Frame,
}

/// What happened to the device on its own (`EngineHost::take_device_events`; the Stage 5 feed toasts
/// them).
#[derive(Clone, Debug, PartialEq)]
pub enum DeviceEvent {
    /// The device stopped (unplugged, a driver reset): the engine, its loops and its slots stay.
    Lost { backend: AudioBackend, reason: String },
    /// The same device came back after a loss (ASIO rebuilt from its cache, the WASAPI default again).
    Recovered(DeviceStatus),
    /// Another device took over after a loss.
    Fallback(DeviceStatus),
    /// Share output's endpoint stopped: the mirror is off and the pick forgotten.
    ShareLost { reason: String },
}

/// Counters the callbacks and pipes bump; every one stays 0 in a clean run (the Stage 4 soak).
#[derive(Default)]
pub struct IoCounters {
    pub callbacks: AtomicU64,
    /// Callback entries more than 1.5 periods after the previous one.
    pub gaps: AtomicU64,
    /// cpal's non-fatal `Xrun` reports (an ASIO overload, a WASAPI glitch).
    pub xruns: AtomicU64,
    /// The callback found the engine locked and played silence.
    pub lock_misses: AtomicU64,
    /// ASIO: the output callback ran without its cycle's input (input and output out of step).
    pub duplex_faults: AtomicU64,
    /// WASAPI join: the output callback found too few input frames (zero-filled), or the input ring
    /// was full (frames dropped).
    pub join_starves: AtomicU64,
    pub join_overruns: AtomicU64,
    /// Share output: the mirror stream found its ring short, or the engine found it full.
    pub share_starves: AtomicU64,
    pub share_overruns: AtomicU64,
    /// `EngineHost::send` found the command ring full.
    pub commands_full: AtomicU64,
    /// A panic caught in the callback (the engine is silent until a new one replaces it).
    pub panics: AtomicU64,
    /// Allocations inside the callback's guard (DEV builds: `host::rt_alloc`).
    pub rt_allocs: AtomicU64,
}

/// A plain copy of the counters and the engine's own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IoDiag {
    pub callbacks: u64,
    pub gaps: u64,
    pub xruns: u64,
    pub lock_misses: u64,
    pub duplex_faults: u64,
    pub join_starves: u64,
    pub join_overruns: u64,
    pub share_starves: u64,
    pub share_overruns: u64,
    pub commands_full: u64,
    pub panics: u64,
    pub rt_allocs: u64,
    pub engine: lf_engine::Diag,
}

/// What the callback holds under the engine lock.
pub(crate) struct Rt {
    pub(crate) engine: Option<Engine>,
    /// A panic was caught in the callback: the engine stays silent until a new one replaces it.
    pub(crate) faulted: bool,
    /// ASIO: this cycle's input, copied by the input callback for the output callback, and each
    /// side's cycle count (equal after a whole cycle).
    pub(crate) handoff: Vec<f32>,
    pub(crate) handoff_len: usize,
    pub(crate) in_cycles: u64,
    pub(crate) out_cycles: u64,
    /// Share output's tap, and the handoff it arrives on while streams run (`None` without an owner).
    pub(crate) tap: Option<Box<dyn Tap>>,
    pub(crate) taps: Option<TapEnd>,
}

/// The engine's non-RT ends, replaced with the engine (a sample-rate change builds a new one).
pub(crate) struct Ends {
    pub(crate) commands: Producer<TimedCommand>,
    pub(crate) events: Consumer<Event>,
}

/// State shared by the device owner, the callbacks, the slot hosts and the command threads.
pub(crate) struct Core {
    /// The callback only `try_lock`s it.
    pub(crate) rt: Mutex<Rt>,
    pub(crate) ends: Mutex<Option<Ends>>,
    /// Each slot's port into the current engine (`None` before the first device opens).
    pub(crate) ports: [Mutex<Option<SlotPort>>; SLOT_COUNT],
    /// A unit is in the engine for this slot (set by a successful install, cleared when it comes back).
    pub(crate) occupied: [AtomicBool; SLOT_COUNT],
    /// Units the device side took out on its own (a rebuild), waiting for their plugin owner.
    pub(crate) evicted: [Mutex<Option<Box<dyn SlotProcessor>>>; SLOT_COUNT],
    /// A device callback runs or is about to: the owner sets it just before the streams start and
    /// clears it just after they drop. While it is false, a slot host may take the engine lock itself
    /// (`SlotHost`); while it is true, it waits for the callback to service its port.
    pub(crate) running: AtomicBool,
    /// The engine's sample rate (0 before the first device opens).
    pub(crate) rate: AtomicU32,
    pub(crate) max_block: AtomicU32,
    pub(crate) clock: FrameClock,
    pub(crate) counters: IoCounters,
    /// The device frame the next output callback renders from: only the output callback writes it,
    /// and it carries over a switch, so the loops resume in place.
    pub(crate) frame: AtomicI64,
    /// The alignment the output callback last rendered with (`DeviceStatus`).
    pub(crate) align_frames: AtomicI64,
    pub(crate) input_frames: AtomicI64,
    /// The engine's own counters, mirrored each callback so `EngineHost::diag` never takes the lock
    /// (a reader holding it would make the callback miss).
    pub(crate) engine_diag: EngineDiag,
    /// The device that runs (the owner writes it).
    pub(crate) device: Mutex<Option<DeviceStatus>>,
    pub(crate) device_events: Mutex<Vec<DeviceEvent>>,
    /// The device owner (`None` for a core without one: the test device).
    pub(crate) owner: Mutex<Option<OwnerLink>>,
}

impl Core {
    pub(crate) fn new() -> Core {
        Core {
            rt: Mutex::new(Rt {
                engine: None,
                faulted: false,
                handoff: vec![0.0; MAX_DEVICE_BLOCK],
                handoff_len: 0,
                in_cycles: 0,
                out_cycles: 0,
                tap: None,
                taps: None,
            }),
            ends: Mutex::new(None),
            ports: std::array::from_fn(|_| Mutex::new(None)),
            occupied: std::array::from_fn(|_| AtomicBool::new(false)),
            evicted: std::array::from_fn(|_| Mutex::new(None)),
            running: AtomicBool::new(false),
            rate: AtomicU32::new(0),
            max_block: AtomicU32::new(0),
            clock: FrameClock::new(),
            counters: IoCounters::default(),
            frame: AtomicI64::new(0),
            align_frames: AtomicI64::new(0),
            input_frames: AtomicI64::new(0),
            engine_diag: EngineDiag::default(),
            device: Mutex::new(None),
            device_events: Mutex::new(Vec::new()),
            owner: Mutex::new(None),
        }
    }

    pub(crate) fn rate(&self) -> Option<u32> {
        Some(self.rate.load(Relaxed)).filter(|&r| r != 0)
    }
}

/// `lf_engine::Diag` as atomics: the callback stores, `EngineHost::diag` loads.
#[derive(Default)]
pub(crate) struct EngineDiag {
    events_dropped: AtomicU64,
    commands_dropped: AtomicU64,
    xruns: AtomicU64,
    slot_events_dropped: AtomicU64,
    slot_protocol_errors: AtomicU64,
}

impl EngineDiag {
    pub(crate) fn store(&self, d: &lf_engine::Diag) {
        self.events_dropped.store(d.events_dropped, Relaxed);
        self.commands_dropped.store(d.commands_dropped, Relaxed);
        self.xruns.store(d.xruns, Relaxed);
        self.slot_events_dropped.store(d.slot_events_dropped, Relaxed);
        self.slot_protocol_errors.store(d.slot_protocol_errors, Relaxed);
    }

    fn load(&self) -> lf_engine::Diag {
        lf_engine::Diag {
            events_dropped: self.events_dropped.load(Relaxed),
            commands_dropped: self.commands_dropped.load(Relaxed),
            xruns: self.xruns.load(Relaxed),
            slot_events_dropped: self.slot_events_dropped.load(Relaxed),
            slot_protocol_errors: self.slot_protocol_errors.load(Relaxed),
        }
    }
}

/// The running device, with the alignment the callback renders with now.
fn status(core: &Core) -> Option<DeviceStatus> {
    let mut status = core.device.lock().ok()?.clone()?;
    status.align_frames = core.align_frames.load(Relaxed);
    status.input_frames = core.input_frames.load(Relaxed);
    Some(status)
}

/// How long a caller waits for the owner: an open builds an engine and starts a device.
const OPEN_TIMEOUT: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// MMCSS "Pro Audio" for the calling thread: every engine, join and share callback calls it once, on
/// its first entry (cpal's `realtime` feature stays off: `Cargo.toml`). False when Windows refused.
pub(crate) fn promote_pro_audio() -> bool {
    use windows::core::w;
    use windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW;
    let mut task_index: u32 = 0;
    // SAFETY: FFI into avrt.dll; `w!` is a 'static NUL-terminated wide literal. The handle is never
    // reverted: the thread belongs to cpal and ends with its stream.
    unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index).is_ok() }
}

/// The process-wide handle on the native engine's device side. Cheap to clone; every method is
/// callable from any non-RT thread.
#[derive(Clone)]
pub struct EngineHost {
    pub(crate) core: Arc<Core>,
}

impl EngineHost {
    /// Spawn the device owner thread. No device opens and no engine exists until [`EngineHost::open`].
    /// The owner runs until [`EngineHost::shutdown`].
    pub fn new(config: HostConfig) -> EngineHost {
        EngineHost::with_driver(config, cpal_driver::CpalDriver)
    }

    /// A host whose owner opens devices through `driver` (the tests' fake).
    pub(crate) fn with_driver<D: Driver>(config: HostConfig, driver: D) -> EngineHost {
        let core = Arc::new(Core::new());
        let link = owner::spawn(core.clone(), config, driver).expect("spawn the device owner thread");
        *core.owner.lock().unwrap_or_else(|e| e.into_inner()) = Some(link);
        EngineHost { core }
    }

    /// Ask the owner and wait for its answer (at most `timeout`).
    fn ask<T>(&self, op: &str, timeout: Duration, request: impl FnOnce(SyncSender<Result<T, String>>) -> Request) -> Result<T, String> {
        let tx = match self.core.owner.lock() {
            Ok(owner) => owner.as_ref().map(|o| o.tx.clone()),
            Err(_) => None,
        };
        let tx = tx.ok_or_else(|| "the audio device owner is not running".to_string())?;
        let (reply_tx, reply_rx) = sync_channel(1);
        tx.send(request(reply_tx)).map_err(|_| "the audio device owner is gone".to_string())?;
        match reply_rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err(format!("{op} timed out after {} s", timeout.as_secs())),
            Err(RecvTimeoutError::Disconnected) => Err("the audio device owner stopped".to_string()),
        }
    }

    /// Open a device, or switch to one: blocks until its streams run or the open fails (≤ 15 s). The
    /// first open builds the engine at the device's rate; a later open at another rate builds a new
    /// engine and evicts the plugin units into their slot hosts. A switch that fails to start reopens
    /// the device it replaced; a request for the running device only changes its channel.
    pub fn open(&self, request: DeviceRequest) -> Result<DeviceStatus, String> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let result = self.ask("open", OPEN_TIMEOUT, |reply| Request::Open(request, cancelled.clone(), reply));
        if result.is_err() {
            // A result that arrives after the timeout is the owner's to undo.
            cancelled.store(true, std::sync::atomic::Ordering::Release);
        }
        result
    }

    /// Stop the device (loops pause in place; a take in flight punches out, STATUS E3).
    pub fn close(&self) -> Result<(), String> {
        self.ask("close", REQUEST_TIMEOUT, Request::Close)
    }

    /// Change the capture channel without rebuilding a stream.
    pub fn set_input_channel(&self, channel: Option<u32>) -> Result<(), String> {
        self.ask("set_input_channel", REQUEST_TIMEOUT, |reply| Request::SetInputChannel(channel, reply))
    }

    /// Share output (STATUS E2): mirror the post-limiter master to this WASAPI render endpoint while
    /// ASIO plays (`None` = off). On WASAPI no mirror opens: app capture takes the main output. The pick
    /// is remembered across switches; an endpoint that fails or dies is forgotten (`DeviceEvent::ShareLost`).
    pub fn set_share(&self, endpoint: Option<String>) -> Result<(), String> {
        self.ask("set_share", REQUEST_TIMEOUT, |reply| Request::SetShare(endpoint, reply))
    }

    pub fn status(&self) -> Option<DeviceStatus> {
        status(&self.core)
    }

    /// What happened to the device on its own since the last call, oldest first.
    pub fn take_device_events(&self) -> Vec<DeviceEvent> {
        self.core.device_events.lock().map(|mut events| std::mem::take(&mut *events)).unwrap_or_default()
    }

    /// Queue a command for the engine. Err when no engine exists yet or the ring is full (counted).
    pub fn send(&self, command: TimedCommand) -> Result<(), String> {
        let mut ends = self.core.ends.lock().map_err(|_| "engine ends poisoned".to_string())?;
        let ends = ends.as_mut().ok_or_else(|| "no audio device is open".to_string())?;
        ends.commands.push(command).map_err(|_| {
            self.core.counters.commands_full.fetch_add(1, Relaxed);
            "the engine's command ring is full".to_string()
        })
    }

    /// Move the engine's events into `out`.
    pub fn drain_events(&self, out: &mut Vec<Event>) {
        if let Ok(mut ends) = self.core.ends.lock() {
            if let Some(ends) = ends.as_mut() {
                while let Ok(e) = ends.events.pop() {
                    out.push(e);
                }
            }
        }
    }

    /// The engine's sample rate, once a device opened.
    pub fn rate(&self) -> Option<u32> {
        self.core.rate()
    }

    /// A plugin owner's handle on `slot` (panics on a slot ≥ `SLOT_COUNT`).
    pub fn slot(&self, slot: usize) -> SlotHost {
        assert!(slot < SLOT_COUNT, "no slot {slot}");
        SlotHost::new(self.core.clone(), slot)
    }

    pub fn frame_clock(&self) -> FrameClock {
        self.core.clock.clone()
    }

    pub fn diag(&self) -> IoDiag {
        let c = &self.core.counters;
        IoDiag {
            callbacks: c.callbacks.load(Relaxed),
            gaps: c.gaps.load(Relaxed),
            xruns: c.xruns.load(Relaxed),
            lock_misses: c.lock_misses.load(Relaxed),
            duplex_faults: c.duplex_faults.load(Relaxed),
            join_starves: c.join_starves.load(Relaxed),
            join_overruns: c.join_overruns.load(Relaxed),
            share_starves: c.share_starves.load(Relaxed),
            share_overruns: c.share_overruns.load(Relaxed),
            commands_full: c.commands_full.load(Relaxed),
            panics: c.panics.load(Relaxed),
            rt_allocs: c.rt_allocs.load(Relaxed),
            engine: self.core.engine_diag.load(),
        }
    }

    /// Close the device and join the owner thread. The engine and its slots drop on the owner thread
    /// (every plugin unit must have been removed first: `SlotHost::remove`). Later requests fail.
    pub fn shutdown(&self) {
        let link = self.core.owner.lock().ok().and_then(|mut owner| owner.take());
        let Some(OwnerLink { tx, join }) = link else { return };
        let (done_tx, done_rx) = sync_channel(1);
        let answered = tx.send(Request::Shutdown(done_tx)).is_ok() && done_rx.recv_timeout(OPEN_TIMEOUT).is_ok();
        if answered || join.is_finished() {
            let _ = join.join();
        } else {
            log::error!("[engine_io] the device owner did not stop within {} s; leaving it", OPEN_TIMEOUT.as_secs());
        }
    }
}
