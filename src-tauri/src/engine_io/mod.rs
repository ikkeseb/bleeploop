//! engine_io: the device side of the native engine (`docs/plans/native-engine.md` § Stage 4). Dormant
//! until the plan's Stage 6 flip: a DEV probe drives it, and nothing on the live line calls it. This doc
//! is the module's briefing.
//!
//! [`EngineHost`] is the process-wide handle: one device owner thread serializes every device
//! transition (open, backend switch, channel change, loss, close); the engine ([`lf_engine::Engine`])
//! sits in one `Mutex` that the device callback only `try_lock`s (a miss plays silence and counts) and
//! that the owner takes only with both streams dropped, so the engine and the plugin slots outlive
//! device switches and loss. The callback body runs under `catch_unwind` inside the lock guard: a caught
//! panic latches a fault and plays silence from then on, the `Mutex` is never poisoned, and nothing
//! unwinds into asio-sys's `extern "C"` bufferSwitch.
//!
//! # Module map
//!
//! | Module | Owns |
//! |---|---|
//! | this file | [`EngineHost`] and the state its threads share ([`Core`]) |
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
//! - **The callback never allocates, logs, locks (beyond its `try_lock`) or waits.** Counters are
//!   atomics the owner reads; the owner logs.
//! - **cpal is pinned at `=0.18.1`:** the callback order the Stage 1 A1 run proved is read from
//!   asio-sys 0.3.0; 0.18.2 moves to asio-sys 0.4.0 and windows 0.62 and needs its own A1 rerun.

pub mod frame_clock;
pub mod midi;
pub(crate) mod pipes;
pub mod share;
pub mod slot_host;
#[cfg(test)]
pub(crate) mod test_rig;

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use lf_engine::grid::Frame;
use lf_engine::{Engine, Event, SlotPort, SlotProcessor, TimedCommand, SLOT_COUNT};
use rtrb::{Consumer, Producer};

use crate::audio_output::AudioBackend;

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
    /// A panic caught in the callback (the engine is silent from then on).
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
    /// A device callback runs: the owner sets it after the streams start and clears it before it drops
    /// them. While it is false, a slot host may take the engine lock itself (`SlotHost`).
    pub(crate) running: AtomicBool,
    /// The engine's sample rate (0 before the first device opens).
    pub(crate) rate: AtomicU32,
    pub(crate) max_block: AtomicU32,
    pub(crate) clock: FrameClock,
    pub(crate) counters: IoCounters,
}

impl Core {
    pub(crate) fn new() -> Core {
        Core {
            rt: Mutex::new(Rt { engine: None }),
            ends: Mutex::new(None),
            ports: std::array::from_fn(|_| Mutex::new(None)),
            occupied: std::array::from_fn(|_| AtomicBool::new(false)),
            evicted: std::array::from_fn(|_| Mutex::new(None)),
            running: AtomicBool::new(false),
            rate: AtomicU32::new(0),
            max_block: AtomicU32::new(0),
            clock: FrameClock::new(),
            counters: IoCounters::default(),
        }
    }

    pub(crate) fn rate(&self) -> Option<u32> {
        Some(self.rate.load(Relaxed)).filter(|&r| r != 0)
    }
}

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
    pub fn new(config: HostConfig) -> EngineHost {
        let _ = config;
        todo!("engine_io lane: spawn the device owner")
    }

    /// Open a device, or switch to one: blocks until its streams run or the open fails (≤ 15 s). The
    /// first open builds the engine at the device's rate; a later open at another rate builds a new
    /// engine and evicts the plugin units into their slot hosts.
    pub fn open(&self, request: DeviceRequest) -> Result<DeviceStatus, String> {
        let _ = request;
        todo!("engine_io lane")
    }

    /// Stop the device (loops pause in place; a take in flight punches out, STATUS E3).
    pub fn close(&self) -> Result<(), String> {
        todo!("engine_io lane")
    }

    /// Change the capture channel without rebuilding a stream.
    pub fn set_input_channel(&self, channel: Option<u32>) -> Result<(), String> {
        let _ = channel;
        todo!("engine_io lane")
    }

    /// Share output (STATUS E2): mirror the post-limiter master to this WASAPI render endpoint while
    /// ASIO plays (`None` = off). On WASAPI no mirror opens: app capture takes the main output.
    pub fn set_share(&self, endpoint: Option<String>) -> Result<(), String> {
        let _ = endpoint;
        todo!("engine_io lane")
    }

    pub fn status(&self) -> Option<DeviceStatus> {
        todo!("engine_io lane")
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
        todo!("engine_io lane")
    }

    /// Close the device and join the owner thread. The engine and its slots drop on the owner thread
    /// (every plugin unit must have been removed first: `SlotHost::remove`).
    pub fn shutdown(&self) {
        todo!("engine_io lane")
    }
}
