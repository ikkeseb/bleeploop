//! engine_io: the device side of the native engine. The app
//! always runs it (`mode`; the UI drives it over `wire` and the feed), and a DEV probe drives it
//! headless. This doc is the module's briefing.
//!
//! [`EngineHost`] is the process-wide handle: one device owner thread serializes every device
//! transition (open, backend switch, a slot's channel change, an ASIO driver switch, loss, close); the
//! engine ([`lf_engine::Engine`]) sits in one `Mutex` that the device callback only `try_lock`s (a miss plays silence and counts) and
//! that the owner takes only with both streams dropped, so the engine and the plugin slots outlive
//! device switches and loss. The callback body runs under `catch_unwind` inside the lock guard: a caught
//! panic latches a fault and plays silence until the owner replaces the engine, the `Mutex` is never
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
//! | `fpu` | the float mode every audio callback runs in: flush-to-zero and denormals-are-zero |
//! | `frame_clock` | [`FrameClock`]: the callback's (time, frame) stamp, whether a device runs, the feed's anchor; DEV, the stamps' history a frame turns into time through |
//! | `pipes` | [`pipes::PullPipe`]: frames pushed on one clock, pulled resampled on another (the WASAPI join, Share output) |
//! | `feed` | the feed: what the UI reads back (events, device, status, anchor, meter, waveforms), on its own thread |
//! | `mode` | engine mode: the managed host, the tone store's folder, native MIDI's start and stop, the `engine_*` Tauri commands, shutdown on exit |
//! | `midi_mode` | native MIDI's Tauri commands: the UI's one ordered input path (`input_send`: its engine commands, through native MIDI's routing, and its note input), the document's subscription and input epoch (`midi_subscribe`), the learn UI's `midi_*` calls |
//! | `plugins` | engine mode's plugin slots: the `plugin_*` commands routed to the engine slot owners, and tone recall's (`host/tone.rs`) |
//! | `session` | a session's bytes to and from the engine: the snapshot the UI saves (an export's with the wet master, rendered offline from the snapshot, its lanes' mix and the kept master volume and mute), the load it imports (each lane with its mix) |
//! | `settings` | the last value of every setting, replayed into each new engine; a lane's mix as the engine applied it (its `Event::Mix`) |
//! | `share` | Share output: the post-limiter master mirrored to a WASAPI endpoint while ASIO plays |
//! | `midi` | native MIDI, the app's only MIDI path: ports and hot-plug, parse, MIDI learn and its stored bindings, the one note router for every note source, the ordered input queue into [`EngineHost::send`] and its part in a rebuild ([`RebuildHook`]) |
//! | `midi_bench` | DEV: the MIDI latency benchmark (a loopback sender, arrival stamps, the applied-note record's report) and the `settings`/`ends` lock waits, each run only when an environment variable asks |
//! | `probe` | DEV: `app.exe --probe-engine`, the device side on real hardware (soak, switches, plugin swaps) |
//! | `tone` | DEV: the probe's loopback tone (`--tone`): its hook in the output callback, the detector, the long and late callbacks' log |
//! | `wire` | the JSON wire to the UI: the serde mirror of the engine's commands and events, the feed frame |
//!
//! # Rules
//!
//! - **One clock: the output callback's frame counter.** ASIO: input built first, output second,
//!   always as a pair, and neither plays until both are built (a playing input can deadlock the
//!   output build: `cpal_driver`'s `start`). asio-sys 0.3.0 runs the registered callbacks in build
//!   order in one bufferSwitch; a run's first output callback takes the input's cycle count,
//!   and from then on a miss is a duplex-order fault, counted. WASAPI: the output callback is the clock; the input joins through a ring and a
//!   resampler with a drift controller. Every callback thread is promoted to MMCSS Pro Audio on first
//!   entry.
//! - **Each plugin slot reads its own capture channel** (`DeviceRequest::input_channels`; auto is input
//!   2 on a device with two or more). An open never fails on a pick: a slot whose pick the device lacks
//!   (saved on a driver with more inputs) reads auto, and `DeviceStatus::input_channels` says what each
//!   slot reads; only a change on the running device refuses one. The input callback publishes one
//!   mono stream per slot, from the picks in `Run` (atomics, changed in place): ASIO copies each slot's channel into its own handoff,
//!   WASAPI pushes them interleaved through the one join pipe. The engine hands each slot its own
//!   (`Engine::process_inputs`); the meter takes the louder.
//! - **The rate pick (`DeviceRequest::sample_rate`, one of [`SAMPLE_RATES`]; `None` = the device's
//!   own) never fails an open either.** ASIO runs it when the driver can (`AsioCache::sample_rates`, read
//!   by the startup probe; cpal sets the driver's rate at the stream build), else the driver's own,
//!   logged (`transition::open_rate`). WASAPI opens at the output endpoint's own rate whatever the pick:
//!   cpal 0.18.1 refuses an output format the endpoint does not run natively (an `IsFormatSupported`
//!   check ahead of its AUTOCONVERTPCM init; 0.18.2 drops it), so Windows' Sound settings set it. A
//!   pick at another rate than the engine's is a new engine, refused while it holds audio (below);
//!   Share output resamples from whatever rate runs. `DeviceStatus::sample_rate` is what runs.
//! - **The frame counter pauses across a switch.** A backend switch or a fallback continues the
//!   counter where the last callback left it, so loops resume in place. It counts the frames the
//!   device took: a late wake is no loss. Only a WASAPI buffer found empty jumps it, by what the device
//!   played dry (the join drops as much input); that, every xrun and the block after a lock miss flag
//!   `ProcessContext::xrun`. A block whose input did not wholly reach the engine flags
//!   `ProcessContext::damaged`: a duplex fault's, and on the join a starve or trim after its startup
//!   priming and every pull until an overrun's seam is surely played (`pipes`), each with the block
//!   after it (the resampler carries a few frames over). The take or layer the gap overlaps is rejected.
//! - **Every stop fades and punches out.** A switch or close ramps the output to silence (10 ms, then
//!   two silent callbacks: a dropped ASIO stream leaves the driver playing its last two buffers), drops
//!   the output then the input, and punches out a take in flight (STATUS E3). A loss drops at once. A
//!   new sample rate builds a new engine: the plugin units go back to their owners (`SlotHost`), and the
//!   loops go with the old engine.
//! - **A loss keeps the engine and its slots.** An error callback latches and the owner drops the
//!   streams: an ASIO driver rebuilds from its cache, else the owner falls back to the WASAPI default;
//!   a lost WASAPI endpoint falls back to the default endpoint (`transition`'s fallbacks). cpal's
//!   `DeviceChanged` and `RealtimeDenied` are not losses (cpal 0.18.1 documents both as non-fatal).
//! - **Loops never leave on a player's switch unasked.** An open that would build an engine at another
//!   rate while this one holds audio (any lane not EMPTY: a loop, a take in flight or armed, a kept
//!   RETAKE pass) is refused with [`OpenError::RateChange`] (the rate is known from `Driver::resolve`,
//!   before anything stops) until the UI confirms and opens again with `force`. The owner's own reopens
//!   (a loss's recovery or fallback, a replaced engine) go ahead; a loss's fallback that rebuilds the
//!   engine at another rate reports [`DeviceEvent::LoopsDropped`], whether or not that device then
//!   starts, and the UI keeps the loops in its recovery (`src/session/autosave.ts`).
//! - **`Core::running` is up from just before the streams start until just after they drop,** so a
//!   slot host never takes the engine lock from under a callback (it waits on its port instead). The
//!   owner raises it under the engine lock, and a slot host checks it again once it holds the lock.
//! - **A fault replaces the engine.** A panic caught under the engine lock (a callback, the idle slot
//!   service, a punch-out) latches `Core::fault`; the owner then builds a new engine at the same rate
//!   and restarts the device that ran. The loops go with the faulted engine, its units back to their
//!   owners (`DeviceEvent::EngineFaulted`). A second fault within `FAULT_HOLDOFF` stays silent.
//! - **The callback never allocates, logs, locks (beyond its `try_lock`) or waits.** Counters are
//!   atomics the owner reads; the owner logs.
//! - **cpal is pinned at `=0.18.1`:** the callback order the Stage 1 A1 run proved is read from
//!   asio-sys 0.3.0; 0.18.2 moves to asio-sys 0.4.0 and windows 0.62 and needs its own A1 rerun.
//!
//! # Tests
//!
//! Everything here runs without hardware in `cargo test`: the transition kernel's tables, the device
//! owner and the callbacks on the fake driver (`tests.rs`), the install/remove/restart handshake with
//! the fixture plugins in a rendering engine (`host/`'s restart fixtures), the pipe matrix at ±400 ppm
//! (`pipes.rs`), native MIDI on a recording engine and through a real rebuild (`midi`), its commands'
//! routing with no port open (`midi_mode`), the wire fixtures (`wire.rs`: the engine's and MIDI's). Code only a device or a real plugin can run is compile-checked (`--features
//! asio` too); on the rig, `pnpm native:engine` runs `probe.rs`'s bar.

mod callback;
mod cpal_driver;
mod driver;
#[cfg(test)]
mod fake_driver;
mod feed;
mod fpu;
pub mod frame_clock;
pub mod midi;
pub mod midi_mode;
#[cfg(debug_assertions)]
pub mod midi_bench;
pub mod mode;
mod owner;
pub(crate) mod pipes;
mod plugins;
#[cfg(debug_assertions)]
pub(crate) mod probe;
mod session;
mod settings;
pub mod share;
pub mod slot_host;
#[cfg(debug_assertions)]
pub(crate) mod tone;
#[cfg(test)]
pub(crate) mod test_rig;
#[cfg(test)]
mod tests;
mod transition;
pub mod wire;

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering::{AcqRel, Acquire, Relaxed}};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender};
use std::sync::{Arc, LockResult, Mutex, MutexGuard};
use std::time::Duration;

use lf_engine::grid::Frame;
use lf_engine::{Command, Engine, Event, LaneState, Overview, SessionPort, SlotPort, SlotProcessor, TimedCommand, SLOT_COUNT, TRACK_COUNT};
use rtrb::{Consumer, Producer};
use serde::{Deserialize, Serialize};

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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", from = "RequestWire")]
pub struct DeviceRequest {
    pub backend: AudioBackend,
    /// WASAPI capture device id (`audio_input::list_input_devices`); `None` = the default. ASIO uses
    /// the cached duplex driver (`audio_output::asio_cache`) and ignores both ids.
    pub input: Option<String>,
    /// WASAPI render device id (`audio_output::list_output_devices`); `None` = the default.
    pub output: Option<String>,
    /// Each plugin slot's capture channel (0-based); `None` = auto (`transition::input_channel`). On
    /// the wire `inputChannels: [a, b]`; a request with one `inputChannel` instead sets both slots.
    pub input_channels: [Option<u32>; SLOT_COUNT],
    /// Frames per device callback; `None` = the driver's default.
    pub buffer: Option<u32>,
    /// The engine's sample rate, one of [`SAMPLE_RATES`]; `None` = the device's own. Applies where the
    /// device runs it (`transition::open_rate`); `DeviceStatus::sample_rate` says what runs. On the wire
    /// `sampleRate`; a request without it asks for the device's own.
    pub sample_rate: Option<u32>,
}

/// The rates a player can pick (`DeviceRequest::sample_rate`).
pub const SAMPLE_RATES: [u32; 2] = [44_100, 48_000];

/// A [`DeviceRequest`] as it arrives: each slot's channel, or one channel for both.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RequestWire {
    backend: AudioBackend,
    input: Option<String>,
    output: Option<String>,
    #[serde(default)]
    input_channels: Option<[Option<u32>; SLOT_COUNT]>,
    #[serde(default)]
    input_channel: Option<u32>,
    buffer: Option<u32>,
    #[serde(default)]
    sample_rate: Option<u32>,
}

impl From<RequestWire> for DeviceRequest {
    fn from(w: RequestWire) -> DeviceRequest {
        let input_channels = w.input_channels.unwrap_or([w.input_channel; SLOT_COUNT]);
        DeviceRequest { backend: w.backend, input: w.input, output: w.output, input_channels, buffer: w.buffer, sample_rate: w.sample_rate }
    }
}

/// The device that runs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceStatus {
    pub backend: AudioBackend,
    pub sample_rate: u32,
    /// Frames per output callback (the driver's; a WASAPI callback may vary around it).
    pub block: u32,
    /// Empty while the input does not run.
    pub input_name: String,
    pub output_name: String,
    /// The input runs. False: WASAPI plays output only (no capture endpoint, or its stream did not
    /// open, e.g. a microphone Windows' privacy settings block) and the engine's input is silence.
    pub input_open: bool,
    /// The capture channel each plugin slot reads (0-based): its pick, or auto where it has none or the
    /// device lacks it.
    pub input_channels: [u32; SLOT_COUNT],
    /// Input plus output latency in frames (`ProcessContext::align_frames`), and its input side.
    pub align_frames: Frame,
    pub input_frames: Frame,
}

/// What happened to the device on its own (`EngineHost::take_device_events`; the feed carries them).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DeviceEvent {
    /// The device stopped (unplugged, a driver reset): the engine, its loops and its slots stay.
    Lost { backend: AudioBackend, reason: String },
    /// The same device came back after a loss (ASIO rebuilt from its cache, the WASAPI default again).
    Recovered(DeviceStatus),
    /// Another device took over after a loss.
    Fallback(DeviceStatus),
    /// Share output's endpoint stopped: the mirror is off and the pick forgotten.
    ShareLost { reason: String },
    /// The engine panicked and a new one at the same rate replaced it: the loops are gone, the plugin
    /// units went back to their owners.
    EngineFaulted,
    /// A loss's fallback rebuilt the engine at `to` Hz, not its `from`: the loops the old one held left
    /// with it (no resampling), whether or not that device then started. The UI keeps them in its
    /// recovery. `device` is the lost one. Follows the fallback's `Recovered` or `Fallback` when it
    /// started, else its failure.
    LoopsDropped { device: String, from: u32, to: u32 },
}

/// Why [`EngineHost::open`] did not open the device. On the wire a failure is its text, a refusal an
/// object (`{"RateChange":{…}}`), so the UI tells the two apart.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpenError {
    /// `device` runs at `to` Hz and the engine at `from` Hz holds audio (`Core::holds_audio`):
    /// a switch would build a new engine there, and the loops cannot play in it. Open again with
    /// `force` to switch anyway.
    RateChange { device: String, from: u32, to: u32 },
    #[serde(untagged)]
    Failed(String),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::RateChange { device, from, to } => {
                write!(f, "\"{device}\" runs at {to} Hz; the loops were recorded at {from} Hz and cannot play there")
            }
            OpenError::Failed(text) => f.write_str(text),
        }
    }
}

impl From<String> for OpenError {
    fn from(text: String) -> OpenError {
        OpenError::Failed(text)
    }
}

impl From<OpenError> for String {
    fn from(error: OpenError) -> String {
        error.to_string()
    }
}

/// Counters the callbacks and pipes bump. The faults (`IoDiag::faults`) stay 0 in a clean run (the
/// Stage 4 soak); the glitch diagnostics after them only tell one kind of glitch from another.
#[derive(Default)]
pub struct IoCounters {
    pub callbacks: AtomicU64,
    /// WASAPI: the output buffer ran dry; the frame counter skipped what the device played dry, the
    /// join as much input (`callback::dry_frames`). On ASIO the only report is the driver's overload, an xrun.
    pub gaps: AtomicU64,
    /// cpal's non-fatal `Xrun` reports: a WASAPI glitch, an ASIO overload once, by the stream that
    /// reported more (`callback::Run::stream_error`).
    pub xruns: AtomicU64,
    /// The callback found the engine locked and played silence.
    pub lock_misses: AtomicU64,
    /// ASIO: the output callback ran without its cycle's input (input and output out of step).
    pub duplex_faults: AtomicU64,
    /// WASAPI join: the output callback found too few input frames (zero-filled), or the input ring
    /// was full (frames dropped).
    pub join_starves: AtomicU64,
    pub join_overruns: AtomicU64,
    /// WASAPI join: the ring ran over twice its setpoint (the output lost time) and was trimmed back.
    pub join_trims: AtomicU64,
    /// Share output: the mirror stream found its ring short, or the engine found it full, or the
    /// mirror trimmed it back (it lost time).
    pub share_starves: AtomicU64,
    pub share_overruns: AtomicU64,
    pub share_trims: AtomicU64,
    /// `EngineHost::send` found the command ring full.
    pub commands_full: AtomicU64,
    /// A panic caught under the engine lock or while evicting (the owner replaces the engine).
    pub panics: AtomicU64,
    /// Allocations inside the callback's guard (DEV builds: `host::rt_alloc`).
    pub rt_allocs: AtomicU64,
    /// How long each output callback took (`EngineHost::block_load`).
    pub block_load: LoadHistogram,
    /// Diagnostics. ASIO: half-second windows whose earliest wake came half a period or more later
    /// than the window before's, a lasting slip of the driver's phase (`callback::PhaseSlips`), and
    /// the furthest past a period behind that floor a single wake came, in frames (informational: the
    /// rig's USB driver spreads its wakes over more than a period at 64 frames).
    pub asio_phase_slips: AtomicU64,
    pub asio_late_max: AtomicU64,
    /// ASIO: episodes of output callbacks that finished 2.4 periods or more after that floor, their
    /// entry's lag plus their own duration (`callback::LATE_FINISH_PERIODS`, an empirical indicator of
    /// a break, not proof either way); blind through a run's first half second.
    pub asio_late_finishes: AtomicU64,
    /// Output callbacks that handed the device a sample past full scale.
    pub clipped_blocks: AtomicU64,
}

/// Bins of the block-load histogram: bin k counts the output callbacks that took k % up to (k + 1) %
/// of their own block's period; the last bin, everything longer.
pub const LOAD_BINS: usize = 200;

/// The output callbacks' durations as a share of their block's period (the Stage 4 soak's bar: p99.9
/// under 50 %, max under 90 %). A callback at or over 100 % ran past its deadline: [`BlockLoad::over_budget`].
pub struct LoadHistogram {
    bins: [AtomicU64; LOAD_BINS],
}

impl Default for LoadHistogram {
    fn default() -> Self {
        LoadHistogram { bins: std::array::from_fn(|_| AtomicU64::new(0)) }
    }
}

impl LoadHistogram {
    /// Count one callback that took `elapsed` for `frames` at `rate`. Never allocates.
    pub(crate) fn record(&self, elapsed: Duration, frames: usize, rate: u32) {
        if frames == 0 || rate == 0 {
            return;
        }
        let percent = elapsed.as_secs_f64() * rate as f64 * 100.0 / frames as f64;
        self.bins[(percent as usize).min(LOAD_BINS - 1)].fetch_add(1, Relaxed);
    }

    fn snapshot(&self) -> BlockLoad {
        BlockLoad { bins: std::array::from_fn(|k| self.bins[k].load(Relaxed)) }
    }
}

/// A copy of the block-load histogram; [`BlockLoad::since`] gives one phase's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockLoad {
    pub bins: [u64; LOAD_BINS],
}

impl BlockLoad {
    pub fn since(&self, earlier: &BlockLoad) -> BlockLoad {
        BlockLoad { bins: std::array::from_fn(|k| self.bins[k].saturating_sub(earlier.bins[k])) }
    }

    pub fn count(&self) -> u64 {
        self.bins.iter().sum()
    }

    /// The bin the `q` quantile falls in (whole percent of the period, the load under k + 1 %).
    pub fn quantile(&self, q: f64) -> Option<usize> {
        let rank = ((q * self.count() as f64).ceil() as u64).max(1);
        let mut seen = 0;
        self.bins.iter().position(|&c| {
            seen += c;
            seen >= rank
        })
    }

    /// The highest bin with a callback in it.
    pub fn max(&self) -> Option<usize> {
        self.bins.iter().rposition(|&c| c > 0)
    }

    /// The callbacks that took 100 % of their period or more: each ran past its deadline.
    pub fn over_budget(&self) -> u64 {
        self.bins[100..].iter().sum()
    }

    /// p50, p99.9 and max as the bins' bounds in whole percent of the period, then `over_budget=N` when
    /// any callback ran past its period; or "none".
    pub(crate) fn text(&self) -> String {
        let bound = |k: usize| if k == LOAD_BINS - 1 { format!(">={k}%") } else { format!("<{}%", k + 1) };
        let over = match self.over_budget() {
            0 => String::new(),
            n => format!(" over_budget={n}"),
        };
        match (self.quantile(0.5), self.quantile(0.999), self.max()) {
            (Some(p50), Some(p999), Some(max)) => format!("p50{} p99.9{} max{}{over}", bound(p50), bound(p999), bound(max)),
            _ => "none".to_string(),
        }
    }
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
    pub join_trims: u64,
    pub share_starves: u64,
    pub share_overruns: u64,
    pub share_trims: u64,
    pub commands_full: u64,
    pub panics: u64,
    pub rt_allocs: u64,
    pub engine: lf_engine::Diag,
    pub asio_phase_slips: u64,
    pub asio_late_max: u64,
    pub asio_late_finishes: u64,
    pub clipped_blocks: u64,
}

impl IoDiag {
    /// Every counter that stays 0 in a clean run, by name (all but `callbacks`).
    pub(crate) fn faults(&self) -> [(&'static str, u64); 18] {
        [
            ("gaps", self.gaps),
            ("xruns", self.xruns),
            ("lock_misses", self.lock_misses),
            ("duplex_faults", self.duplex_faults),
            ("join_starves", self.join_starves),
            ("join_overruns", self.join_overruns),
            ("join_trims", self.join_trims),
            ("share_starves", self.share_starves),
            ("share_overruns", self.share_overruns),
            ("share_trims", self.share_trims),
            ("commands_full", self.commands_full),
            ("panics", self.panics),
            ("rt_allocs", self.rt_allocs),
            ("engine.events_dropped", self.engine.events_dropped),
            ("engine.commands_dropped", self.engine.commands_dropped),
            ("engine.xruns", self.engine.xruns),
            ("engine.slot_events_dropped", self.engine.slot_events_dropped),
            ("engine.slot_protocol_errors", self.engine.slot_protocol_errors),
        ]
    }

    /// The glitch diagnostics that count, by name: not faults (no probe fails on them), but the release
    /// log and the probe show them beside the faults.
    fn diagnostics(&self) -> [(&'static str, u64); 3] {
        [("asio_phase_slips", self.asio_phase_slips), ("asio_late_finishes", self.asio_late_finishes), ("clipped_blocks", self.clipped_blocks)]
    }

    /// The fault counters, then the diagnostics, that moved since `before`, as `name=delta`, with
    /// `asio_late_max` as its new value when it rose; `None` when none did.
    pub(crate) fn moved_since(&self, before: &IoDiag) -> Option<String> {
        let mut moved: Vec<String> = self
            .faults()
            .into_iter()
            .zip(before.faults())
            .chain(self.diagnostics().into_iter().zip(before.diagnostics()))
            .filter(|(n, b)| n.1 > b.1)
            .map(|(n, b)| format!("{}={}", n.0, n.1 - b.1))
            .collect();
        if self.asio_late_max > before.asio_late_max {
            moved.push(format!("asio_late_max={}", self.asio_late_max));
        }
        (!moved.is_empty()).then(|| moved.join(" "))
    }
}

/// What the callback holds under the engine lock.
pub(crate) struct Rt {
    pub(crate) engine: Option<Engine>,
    /// A panic was caught under the lock: the engine stays silent until the owner replaces it
    /// (`Core::fault` tells the owner).
    pub(crate) faulted: bool,
    /// ASIO: this cycle's input, each slot's channel, copied by the input callback for the output
    /// callback, and each side's cycle count (equal after a whole cycle).
    pub(crate) handoff: [Vec<f32>; SLOT_COUNT],
    pub(crate) handoff_len: usize,
    pub(crate) in_cycles: u64,
    pub(crate) out_cycles: u64,
    /// Share output's tap, and the handoff it arrives on while streams run (`None` without an owner).
    pub(crate) tap: Option<Box<dyn Tap>>,
    pub(crate) taps: Option<TapEnd>,
    /// DEV: the probe's lag phase (`probe::LagRig`), set while no device runs.
    #[cfg(debug_assertions)]
    pub(crate) lag: Option<Box<probe::LagRig>>,
    /// DEV: the probe's loopback tone (`tone::ToneRig`), set while no device runs.
    #[cfg(debug_assertions)]
    pub(crate) tone: Option<Box<tone::ToneRig>>,
}

/// The engine's non-RT ends, replaced with the engine (a sample-rate change builds a new one).
pub(crate) struct Ends {
    pub(crate) commands: Producer<TimedCommand>,
    pub(crate) events: Consumer<Event>,
    pub(crate) overview: Arc<Overview>,
    /// The session port (`session.rs`), out while a snapshot or load uses it.
    pub(crate) session: Option<SessionPort>,
}

/// Who takes `settings` or `ends` (`lock_at`): the input path (`EngineHost::send_all`), the feed
/// (`drain_feed`), or another reader.
#[derive(Clone, Copy, Debug)]
pub(crate) enum LockSite {
    SettingsSend,
    EndsSend,
    SettingsFeed,
    EndsFeed,
    Settings,
    Ends,
}

/// Take `mutex` for `site`: DEV builds time the wait (`midi_bench`'s lock waits); release builds lock.
fn lock_at<T>(mutex: &Mutex<T>, site: LockSite) -> LockResult<MutexGuard<'_, T>> {
    #[cfg(debug_assertions)]
    let began = std::time::Instant::now();
    let guard = mutex.lock();
    #[cfg(debug_assertions)]
    midi_bench::LOCK_WAITS.record(site, began.elapsed());
    #[cfg(not(debug_assertions))]
    let _ = site;
    guard
}

/// State shared by the device owner, the callbacks, the slot hosts and the command threads.
pub(crate) struct Core {
    /// The callback only `try_lock`s it.
    pub(crate) rt: Mutex<Rt>,
    pub(crate) ends: Mutex<Option<Ends>>,
    /// The last value of every setting, replayed into each new engine; a lane's mix as the engine
    /// applied it (`settings`). Taken before `ends`.
    pub(crate) settings: Mutex<settings::Settings>,
    /// Bumped whenever the engine is replaced or dropped (the feed resyncs the UI).
    pub(crate) engine_gen: AtomicU64,
    /// Each slot's port into the current engine (`None` before the first device opens).
    pub(crate) ports: [Mutex<Option<SlotPort>>; SLOT_COUNT],
    /// Whose unit is in the engine for this slot: the installing [`SlotHost`]'s token, set by a
    /// successful install and cleared (0) when the unit comes back; [`slot_host::ORPHAN`] once its owner
    /// gave up on it (`SlotHost::abandon`).
    pub(crate) holder: [AtomicU64; SLOT_COUNT],
    /// The next [`SlotHost`]'s token (from 1).
    pub(crate) next_token: AtomicU64,
    /// `Rt::faulted` was latched: the owner replaces the engine at its next poll.
    pub(crate) fault: AtomicBool,
    /// Units the device side took out on its own (a rebuild), waiting for their plugin owner, with its
    /// token: only that owner's [`SlotHost`] takes one back.
    pub(crate) evicted: [Mutex<Option<(u64, Box<dyn SlotProcessor>)>>; SLOT_COUNT],
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
    /// The input meter since the feed last took it: the slots' capture channels' peak (f32 bits) and
    /// whether a sample reached full scale.
    pub(crate) meter_peak: AtomicU32,
    pub(crate) meter_clip: AtomicBool,
    /// One session job at a time (`session.rs`), and the jobs an earlier call gave up on that are still
    /// in the engine of that generation: (generation, count).
    pub(crate) session_busy: Mutex<(u64, usize)>,
    /// Session jobs this host has handed to an engine: a test waits on it for a job to be enqueued.
    #[cfg(test)]
    pub(crate) session_sent: AtomicU64,
    /// The engine's own counters, mirrored each callback so `EngineHost::diag` never takes the lock
    /// (a reader holding it would make the callback miss).
    pub(crate) engine_diag: EngineDiag,
    /// The device that runs (the owner writes it).
    pub(crate) device: Mutex<Option<DeviceStatus>>,
    pub(crate) device_events: Mutex<Vec<DeviceEvent>>,
    /// The device owner (`None` for a core without one: the test device).
    pub(crate) owner: Mutex<Option<OwnerLink>>,
    /// The input path's part in an engine rebuild (`EngineHost::set_rebuild_hook`).
    pub(crate) rebuild_hook: Mutex<Option<Arc<dyn RebuildHook>>>,
    /// DEV: the probe's tone run, once it set one. The output callback logs its long and late callbacks
    /// there outside the engine lock (`tone::SlowLog`: atomics, no lock).
    #[cfg(debug_assertions)]
    pub(crate) tone: std::sync::OnceLock<Arc<tone::Shared>>,
}

/// What an engine rebuild tells the input path: native MIDI's one queue (`midi`), whose commands were
/// made for the engine being replaced (`docs/ARCHITECTURE.md` § Decided: native MIDI, a rebuild needs
/// no WebView). `owner.rs` `swap_engine` calls it on whatever thread swaps (the device owner), never on
/// the audio thread and never under `settings` or `ends` (the input path sends under its own lock,
/// which comes before them): [`RebuildHook::pause`] before the swap, [`RebuildHook::rebuild`] once the
/// new engine is in, [`RebuildHook::resume`] once its settings replay is queued. It needs nothing of
/// the WebView, so a stalled UI cannot hold a recovery up. Its methods change the input path's own
/// state and nothing else: the caller holds every slot port, so they send nothing and call no UI sink
/// (one that reached `SlotHost::remove` would wait on a lock its own thread holds); the input path's
/// own thread does both afterwards.
pub trait RebuildHook: Send + Sync {
    /// Hold back fresh input and stop sending.
    fn pause(&self);
    /// The engine of generation `generation` replaced the old one. Returns what the input path meant to
    /// send and never did that the settings memory keeps (the latest note target and wheels): recorded
    /// before the replay, so the new engine gets them through the replay alone.
    fn rebuild(&self, generation: u64) -> Vec<Command>;
    /// The replay is queued: input flows again.
    fn resume(&self);
}

/// Pop the event ring of the engine of generation `gen` into `each`. A lane's `Mix`, a `Toggled` and a
/// toggle's `Refused` are also the settings memory's projection of them: drained under `settings`, then `ends` (the order every
/// taker keeps), so no rebuild's replay lands between the pop and the projection.
pub(crate) fn drain(settings: &mut settings::Settings, events: &mut Consumer<Event>, gen: u64, mut each: impl FnMut(Event)) {
    while let Ok(event) = events.pop() {
        match event {
            Event::Mix { frame, lane, mix } => _ = settings.mixed(gen, frame, lane, &mix),
            Event::Toggled { toggle, on, .. } => _ = settings.toggled(gen, toggle, on),
            Event::Refused { reason, .. } => settings.refused(gen, reason),
            _ => {}
        }
        each(event);
    }
}

impl Core {
    pub(crate) fn new() -> Core {
        Core {
            rt: Mutex::new(Rt {
                engine: None,
                faulted: false,
                handoff: std::array::from_fn(|_| vec![0.0; MAX_DEVICE_BLOCK]),
                handoff_len: 0,
                in_cycles: 0,
                out_cycles: 0,
                tap: None,
                taps: None,
                #[cfg(debug_assertions)]
                lag: None,
                #[cfg(debug_assertions)]
                tone: None,
            }),
            ends: Mutex::new(None),
            settings: Mutex::new(settings::Settings::default()),
            engine_gen: AtomicU64::new(0),
            ports: std::array::from_fn(|_| Mutex::new(None)),
            holder: std::array::from_fn(|_| AtomicU64::new(0)),
            next_token: AtomicU64::new(1),
            fault: AtomicBool::new(false),
            evicted: std::array::from_fn(|_| Mutex::new(None)),
            running: AtomicBool::new(false),
            rate: AtomicU32::new(0),
            max_block: AtomicU32::new(0),
            clock: FrameClock::new(),
            counters: IoCounters::default(),
            frame: AtomicI64::new(0),
            align_frames: AtomicI64::new(0),
            input_frames: AtomicI64::new(0),
            meter_peak: AtomicU32::new(0),
            meter_clip: AtomicBool::new(false),
            session_busy: Mutex::new((0, 0)),
            #[cfg(test)]
            session_sent: AtomicU64::new(0),
            engine_diag: EngineDiag::default(),
            device: Mutex::new(None),
            device_events: Mutex::new(Vec::new()),
            owner: Mutex::new(None),
            rebuild_hook: Mutex::new(None),
            #[cfg(debug_assertions)]
            tone: std::sync::OnceLock::new(),
        }
    }

    pub(crate) fn rate(&self) -> Option<u32> {
        Some(self.rate.load(Relaxed)).filter(|&r| r != 0)
    }

    /// Some lane of the engine holds audio, or will: any lane not EMPTY (a loop, a take in flight or
    /// armed, a kept RETAKE pass). Read from its overview's lane states, without the engine lock: a
    /// lane's frames read 0 across a RETAKE pass boundary and through a count-in.
    pub(crate) fn holds_audio(&self) -> bool {
        let ends = lock_at(&self.ends, LockSite::Ends).unwrap_or_else(|e| e.into_inner());
        ends.as_ref().is_some_and(|e| (0..TRACK_COUNT).any(|i| e.overview.lane(i).state != LaneState::Empty))
    }

    /// A panic was caught under the engine lock: silence the engine and tell the owner.
    pub(crate) fn latch_fault(&self, rt: &mut Rt) {
        rt.faulted = true;
        self.fault.store(true, std::sync::atomic::Ordering::Release);
    }

    /// A plain copy of the counters (`EngineHost::diag`; the owner's glitch watch reads it too).
    pub(crate) fn diag(&self) -> IoDiag {
        let c = &self.counters;
        IoDiag {
            callbacks: c.callbacks.load(Relaxed),
            gaps: c.gaps.load(Relaxed),
            xruns: c.xruns.load(Relaxed),
            lock_misses: c.lock_misses.load(Relaxed),
            duplex_faults: c.duplex_faults.load(Relaxed),
            join_starves: c.join_starves.load(Relaxed),
            join_overruns: c.join_overruns.load(Relaxed),
            join_trims: c.join_trims.load(Relaxed),
            share_starves: c.share_starves.load(Relaxed),
            share_overruns: c.share_overruns.load(Relaxed),
            share_trims: c.share_trims.load(Relaxed),
            commands_full: c.commands_full.load(Relaxed),
            panics: c.panics.load(Relaxed),
            rt_allocs: c.rt_allocs.load(Relaxed),
            engine: self.engine_diag.load(),
            asio_phase_slips: c.asio_phase_slips.load(Relaxed),
            asio_late_max: c.asio_late_max.load(Relaxed),
            asio_late_finishes: c.asio_late_finishes.load(Relaxed),
            clipped_blocks: c.clipped_blocks.load(Relaxed),
        }
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
/// A driver switch: a close, the new driver's probe (its own 15-second deadline) and a reopen.
const SWITCH_TIMEOUT: Duration = Duration::from_secs(35);

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
        EngineHost::with_driver(config, cpal_driver::CpalDriver { preopen: true })
    }

    /// A host whose owner opens devices through `driver` (the tests' fake).
    pub(crate) fn with_driver<D: Driver>(config: HostConfig, driver: D) -> EngineHost {
        let core = Arc::new(Core::new());
        let link = owner::spawn(core.clone(), config, driver).expect("spawn the device owner thread");
        *core.owner.lock().unwrap_or_else(|e| e.into_inner()) = Some(link);
        EngineHost { core }
    }

    fn owner_tx(&self) -> Result<std::sync::mpsc::Sender<Request>, String> {
        let tx = match self.core.owner.lock() {
            Ok(owner) => owner.as_ref().map(|o| o.tx.clone()),
            Err(_) => None,
        };
        tx.ok_or_else(|| "the audio device owner is not running".to_string())
    }

    /// Ask the owner and wait for its answer (at most `timeout`).
    fn ask<T>(&self, op: &str, timeout: Duration, request: impl FnOnce(SyncSender<Result<T, String>>) -> Request) -> Result<T, String> {
        let tx = self.owner_tx()?;
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
    /// engine and evicts the plugin units into their slot hosts, and the loops go with the old engine,
    /// so while it holds audio that switch is refused ([`OpenError::RateChange`]) unless `force`. A
    /// switch that fails to start reopens the device it replaced; a request for the running device only
    /// changes its channels. A slot's pick the device lacks reads auto (the status says which).
    pub fn open(&self, request: DeviceRequest, force: bool) -> Result<DeviceStatus, OpenError> {
        // Whoever claims the flag first decides: the owner once the open finished (its result is then
        // this caller's), or this caller on its timeout (a later open is the owner's to undo).
        let claimed = Arc::new(AtomicBool::new(false));
        let tx = self.owner_tx()?;
        let (reply_tx, reply_rx) = sync_channel(1);
        tx.send(Request::Open(request, force, claimed.clone(), reply_tx)).map_err(|_| "the audio device owner is gone".to_string())?;
        match reply_rx.recv_timeout(OPEN_TIMEOUT) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                if claimed.compare_exchange(false, true, AcqRel, Acquire).is_ok() {
                    Err(format!("open timed out after {} s", OPEN_TIMEOUT.as_secs()).into())
                } else {
                    // The owner claimed it at the deadline: its answer is on the way.
                    reply_rx.recv().unwrap_or_else(|_| Err("the audio device owner stopped".to_string().into()))
                }
            }
            Err(RecvTimeoutError::Disconnected) => Err("the audio device owner stopped".to_string().into()),
        }
    }

    /// Stop the device (loops pause in place; a take in flight punches out, STATUS E3).
    pub fn close(&self) -> Result<(), String> {
        self.ask("close", REQUEST_TIMEOUT, Request::Close)
    }

    /// Change `slot`'s capture channel (`None` = auto) without rebuilding a stream; the running
    /// request keeps it, so the owner's reopens (a loss, a fault) keep it too. Refused for a channel the
    /// device lacks (the other slot's stays), and while no device runs.
    pub fn set_slot_input_channel(&self, slot: usize, channel: Option<u32>) -> Result<(), String> {
        self.ask("set_slot_input_channel", REQUEST_TIMEOUT, |reply| Request::SetSlotInputChannel(slot, channel, reply))
    }

    /// Switch the ASIO driver: `switch` replaces the cached one (`audio_output::switch_asio_driver`) on
    /// the owner, between two of its transitions, so no open, reopen or recovery takes the cache while it
    /// changes. A device running on ASIO closes first and opens again after, on the driver then cached,
    /// unless that one runs at another rate while the engine holds audio (the UI's open asks first).
    /// Resolves with `switch`'s report.
    pub fn switch_asio(&self, switch: impl FnOnce() -> Result<crate::asio_startup::AsioStatusReport, String> + Send + 'static) -> Result<crate::asio_startup::AsioStatusReport, String> {
        self.ask("switch_asio", SWITCH_TIMEOUT, |reply| Request::SwitchAsio(Box::new(switch), reply))
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

    /// Queue a command for the engine. A setting the ring takes is also kept and replayed into every new
    /// engine (`settings`), as is one sent before the first open, so it is not lost; a lane's mix is kept
    /// once the engine reports it applied (`Event::Mix`). Err when the ring is full (counted; the setting
    /// is not kept), or for an action while no engine exists yet.
    pub fn send(&self, command: TimedCommand) -> Result<(), String> {
        self.send_all([command])
    }

    /// Queue a batch in order, as one: no engine rebuild's replay lands inside it. A full ring stops it
    /// there (the rest are not sent). While no engine exists every setting in it is kept, a note
    /// release and a `Press` are dropped (nothing can be held, no CLEAR armed), and any other action
    /// makes it an error.
    pub fn send_all(&self, commands: impl IntoIterator<Item = TimedCommand>) -> Result<(), String> {
        let mut settings = lock_at(&self.core.settings, LockSite::SettingsSend).map_err(|_| "engine settings poisoned".to_string())?;
        let mut ends = lock_at(&self.core.ends, LockSite::EndsSend).map_err(|_| "engine ends poisoned".to_string())?;
        let mut refused = false;
        for command in commands {
            match ends.as_mut() {
                Some(ends) => {
                    // Kept once the ring took it: a setting the engine never got must not reach a rebuild
                    // or an export either.
                    ends.commands.push(command).map_err(|_| {
                        self.core.counters.commands_full.fetch_add(1, Relaxed);
                        "the engine's command ring is full".to_string()
                    })?;
                    settings.record(&command.command);
                    settings.pushed(&command.command);
                }
                None => refused |= !settings.record(&command.command) && !matches!(command.command, Command::NoteOff(_) | Command::AllNotesOff | Command::Press),
            }
        }
        if refused { Err("no audio device is open".to_string()) } else { Ok(()) }
    }

    /// The kept settings, in replay order (a reset frame hands them to the UI).
    pub(crate) fn settings(&self) -> Vec<lf_engine::Command> {
        lock_at(&self.core.settings, LockSite::Settings).map(|s| s.replay().collect()).unwrap_or_default()
    }

    /// Each lane's mix as the engine last reported it (`Event::Mix`), for the lanes it has reported.
    pub(crate) fn mixes(&self) -> Vec<Event> {
        lock_at(&self.core.settings, LockSite::Settings).map(|s| s.mixes().collect()).unwrap_or_default()
    }

    /// Move the engine's events into `out`, as `drain_events`; with the generation of the engine they
    /// came from and its overview (`None` while no engine exists), read under the same lock.
    pub(crate) fn drain_feed(&self, out: &mut Vec<Event>) -> (u64, Option<Arc<Overview>>) {
        let mut settings = lock_at(&self.core.settings, LockSite::SettingsFeed).unwrap_or_else(|e| e.into_inner());
        let Ok(mut ends) = lock_at(&self.core.ends, LockSite::EndsFeed) else { return (self.core.engine_gen.load(Acquire), None) };
        let gen = self.core.engine_gen.load(Acquire);
        let Some(ends) = ends.as_mut() else { return (gen, None) };
        drain(&mut settings, &mut ends.events, gen, |e| out.push(e));
        (gen, Some(ends.overview.clone()))
    }

    /// Move the engine's events into `out` (a lane's `Mix` reaches the settings memory on the way).
    pub fn drain_events(&self, out: &mut Vec<Event>) {
        self.drain_feed(out);
    }

    /// The input meter since the last call: the louder slot input's linear peak, and whether a sample
    /// reached full scale.
    pub fn take_meter(&self) -> (f32, bool) {
        let peak = f32::from_bits(self.core.meter_peak.swap(0, Relaxed));
        (peak, self.core.meter_clip.swap(false, Relaxed))
    }

    /// The engine's sample rate, once a device opened.
    pub fn rate(&self) -> Option<u32> {
        self.core.rate()
    }

    /// A plugin owner's handle on `slot` (panics on a slot ≥ `SLOT_COUNT`). Each call is a new owner:
    /// a unit comes back only to the handle that installed it, or a clone of it.
    pub fn slot(&self, slot: usize) -> SlotHost {
        assert!(slot < SLOT_COUNT, "no slot {slot}");
        SlotHost::new(self.core.clone(), slot)
    }

    pub fn frame_clock(&self) -> FrameClock {
        self.core.clock.clone()
    }

    /// Register what an engine rebuild tells the input path ([`RebuildHook`]); `None` removes it.
    pub fn set_rebuild_hook(&self, hook: Option<Arc<dyn RebuildHook>>) {
        *self.core.rebuild_hook.lock().unwrap_or_else(|e| e.into_inner()) = hook;
    }

    pub fn diag(&self) -> IoDiag {
        self.core.diag()
    }

    /// The output callbacks' durations so far (`LoadHistogram`).
    pub fn block_load(&self) -> BlockLoad {
        self.core.counters.block_load.snapshot()
    }

    /// Close the device and join the owner thread. The engine drops on the owner thread; a plugin unit
    /// still in it goes back to its owner first, stopped (`SlotHost::remove` / `take_evicted`). Later
    /// requests fail.
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
