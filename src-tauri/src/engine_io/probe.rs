//! DEV engine probe (`docs/plans/native-engine.md` § Stage 4, the rig gates): the device side on real
//! hardware, headless. `app.exe --probe-engine …` exits before Tauri starts. It opens the device, loads
//! a plugin into each slot asked for, records a loop on lane 0, soaks, then switches backend and buffer
//! size and swaps the plugins while the loop plays, printing each phase's counters. It fails when a
//! counter moved, the device reported an event, the loop stopped at an unchanged rate, a plugin did not
//! come back, an error was logged, or the soak missed the block-load bar.
//!
//! `app.exe --probe-engine <asio|wasapi> <64|128|256|default> [--plugin <slot>=<file.vst3|.clap>]...
//! [--seconds N] [--switches N] [--swaps N] [--in N] [--device <WASAPI name substring>] [--mute]
//! [--lag [--out N] [--no-preopen] [--split <out|in>]]`
//!
//! `--lag` runs only the lag phase instead (the Stage 1 A2 bar on the engine's own open path): a chirp
//! leaves on output `--out` (0-based, default 1) every quarter second (WASAPI: every second) and comes
//! back through a loopback cable on input `--in`; each arrival's lag is compared with the alignment the
//! engine renders with (the driver's input plus output latency). `--no-preopen` opens ASIO without the
//! preopen (`cpal_driver`), for a before-and-after comparison. A WASAPI lag run first prints what
//! WASAPI's own clocks report (`wasapi_clocks`).
//!
//! `--split out|in` (ASIO, with `--device`) splits the WASAPI round trip by its sides, against ASIO's
//! (whose report A2 holds) on QPC: `out` plays the chirps from a WASAPI render client of the `--device`
//! endpoint and ASIO records them; `in` plays them from ASIO and a WASAPI capture client records them.
//! Each arrival is compared with the instant WASAPI's own stamps put it at.
//!
//! The take records input channel `--in` (0-based, default 0) through slot 0, live for the take only:
//! keep that channel off a loopback cable, or the take's monitor feeds back through it. A swap puts the
//! next plugin of the `--plugin` list into a slot (with one, the same plugin again). `--mute` mutes the
//! master, the monitored input included: the device plays silence, so a WASAPI run can share the
//! interface with other apps.

use std::sync::atomic::{AtomicU64, Ordering::{Acquire, Relaxed}};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lf_engine::grid::Frame;
use lf_engine::{Command, Event, LaneInfo, LaneState, TimedCommand, SLOT_COUNT};

use super::{BlockLoad, DeviceRequest, DeviceStatus, EngineHost, HostConfig, IoDiag, LOAD_BINS};
use crate::chirp_lag::{chirp, find_arrivals, median, slope, spread, CHIRP_LEN};
use crate::audio_output::AudioBackend;
use crate::host::engine_slot::{self, EngineSlotEvent, EngineSlotHandle, EventSink, PluginFormat};

/// How long a switch or a swap plays before the probe looks again.
const HOLD: Duration = Duration::from_secs(2);
/// How long a plugin may take to come back into its slot after a switch.
const SLOT_WAIT: Duration = Duration::from_secs(5);
const WAIT: Duration = Duration::from_secs(10);
/// The soak's block-load bar, in whole percent of the period.
const P999_BAR: usize = 50;
const MAX_BAR: usize = 90;

/// Every check, in report order, with its bar.
const CHECKS: [(&str, &str); 8] = [
    ("run", "the device opens and a loop records"),
    ("switch", "every backend and buffer switch starts"),
    ("slots", "every plugin loads, is back in its slot after each switch, reloads on a swap and unloads"),
    ("loop", "the loop plays through every phase at an unchanged rate; no take or pass rejected"),
    ("events", "no device event (loss, fallback, engine fault)"),
    ("counters", "every IoDiag counter but callbacks stays 0"),
    ("load", "soak: block time p99.9 < 50 %, max < 90 % of the period"),
    ("log", "no error logged"),
];

fn say(line: impl AsRef<str>) {
    println!("[engine-probe] {}", line.as_ref());
}

/// The engine's and the plugin owners' log, printed; errors counted (the `log` check).
struct ProbeLog {
    errors: AtomicU64,
}

static LOG: ProbeLog = ProbeLog { errors: AtomicU64::new(0) };

impl log::Log for ProbeLog {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        if record.level() == log::Level::Error {
            self.errors.fetch_add(1, Relaxed);
        }
        say(format!("log {} {}", record.level(), record.args()));
    }

    fn flush(&self) {}
}

struct Args {
    backend: AudioBackend,
    buffer: Option<u32>,
    plugins: Vec<(usize, String)>,
    seconds: f64,
    switches: usize,
    swaps: usize,
    input: u32,
    device: Option<String>,
    mute: bool,
    lag: bool,
    out: usize,
    preopen: bool,
    split: Option<Split>,
}

/// `--split`: which WASAPI side the lag run measures against ASIO.
#[derive(Clone, Copy, PartialEq)]
enum Split {
    Out,
    In,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    const USAGE: &str = "usage: --probe-engine <asio|wasapi> <64|128|256|default> [--plugin <slot>=<file.vst3|.clap>]... \
        [--seconds N] [--switches N] [--swaps N] [--in N] [--device <WASAPI name substring>] [--mute] [--lag [--out N] [--no-preopen] [--split <out|in>]]";
    let backend = match args.first().map(String::as_str) {
        Some("asio") => AudioBackend::Asio,
        Some("wasapi") => AudioBackend::Wasapi,
        _ => return Err(USAGE.into()),
    };
    let buffer = match args.get(1).map(String::as_str) {
        Some("default") => None,
        Some(b @ ("64" | "128" | "256")) => Some(b.parse().unwrap()),
        _ => return Err(USAGE.into()),
    };
    if backend == AudioBackend::Wasapi && buffer.is_some() {
        return Err("WASAPI runs at the audio engine's period: use `default`".into());
    }
    let mut parsed = Args {
        backend,
        buffer,
        plugins: Vec::new(),
        seconds: 60.0,
        switches: 0,
        swaps: 0,
        input: 0,
        device: None,
        mute: false,
        lag: false,
        out: 1,
        preopen: true,
        split: None,
    };
    let mut rest = args[2..].iter();
    while let Some(flag) = rest.next() {
        let mut value = || rest.next().cloned().ok_or_else(|| format!("{flag} needs a value"));
        let number = |v: String| v.parse::<f64>().map_err(|e| format!("{flag}: {e}"));
        match flag.as_str() {
            "--plugin" => {
                let v = value()?;
                let (slot, path) = v.split_once('=').ok_or_else(|| format!("--plugin {v}: expected <slot>=<path>"))?;
                let slot: usize = slot.parse().map_err(|e| format!("--plugin slot {slot}: {e}"))?;
                if slot >= SLOT_COUNT || parsed.plugins.iter().any(|(s, _)| *s == slot) {
                    return Err(format!("--plugin: slot {slot} is taken or not one of 0..{SLOT_COUNT}"));
                }
                parsed.plugins.push((slot, path.to_string()));
            }
            "--seconds" => parsed.seconds = number(value()?)?,
            "--switches" => parsed.switches = number(value()?)? as usize,
            "--swaps" => parsed.swaps = number(value()?)? as usize,
            "--in" => parsed.input = number(value()?)? as u32,
            "--device" => parsed.device = Some(value()?),
            "--mute" => parsed.mute = true,
            "--lag" => parsed.lag = true,
            "--out" => parsed.out = number(value()?)? as usize,
            "--no-preopen" => parsed.preopen = false,
            "--split" => {
                parsed.split = Some(match value()?.as_str() {
                    "out" => Split::Out,
                    "in" => Split::In,
                    other => return Err(format!("--split {other}: expected out or in")),
                })
            }
            _ => return Err(format!("unknown argument {flag}\n{USAGE}")),
        }
    }
    if !(3.0..=3600.0).contains(&parsed.seconds) {
        return Err("the soak runs 3 s .. 60 min".into());
    }
    if parsed.swaps > 0 && parsed.plugins.is_empty() {
        return Err("--swaps needs a --plugin".into());
    }
    if parsed.split.is_some() && !(parsed.lag && backend.is_asio() && parsed.device.is_some()) {
        return Err("--split needs --lag on asio and a --device for the WASAPI side".into());
    }
    Ok(parsed)
}

/// A plugin to load: the first one its bundle lists.
#[derive(Clone)]
struct PluginSpec {
    path: String,
    format: PluginFormat,
    id: String,
    name: String,
}

impl PluginSpec {
    fn scan(path: &str) -> Result<PluginSpec, String> {
        let lower = path.to_ascii_lowercase();
        let format = if lower.ends_with(".vst3") {
            PluginFormat::Vst3
        } else if lower.ends_with(".clap") {
            PluginFormat::Clap
        } else {
            return Err(format!("{path}: not a .vst3 or .clap"));
        };
        let first = crate::host::scan_one(path)?.into_iter().next().ok_or_else(|| format!("{path}: no plugin inside"))?;
        Ok(PluginSpec { path: path.to_string(), format, id: first.id, name: first.name })
    }
}

struct Slot {
    slot: usize,
    /// Index into `Probe::plugins` of what the slot holds (or last held).
    plugin: usize,
    handle: Option<EngineSlotHandle>,
}

/// The requests the probe opens: WASAPI endpoints picked once by name, the capture channel fixed.
struct Devices {
    wasapi_in: Option<String>,
    wasapi_out: Option<String>,
    channel: u32,
}

impl Devices {
    fn new(channel: u32, name: Option<&str>) -> Result<Devices, String> {
        let Some(name) = name.map(str::to_lowercase) else {
            return Ok(Devices { wasapi_in: None, wasapi_out: None, channel });
        };
        let input = crate::audio_input::list_input_devices()?.into_iter().find(|d| d.name.to_lowercase().contains(&name));
        let output = crate::audio_output::list_output_devices()?.into_iter().find(|d| d.name.to_lowercase().contains(&name));
        match (input, output) {
            (Some(i), Some(o)) => {
                say(format!("wasapi endpoints: in '{}', out '{}'", i.name, o.name));
                Ok(Devices { wasapi_in: Some(i.id), wasapi_out: Some(o.id), channel })
            }
            _ => Err(format!("--device {name}: no WASAPI input and output with that in the name")),
        }
    }

    fn request(&self, backend: AudioBackend, buffer: Option<u32>) -> DeviceRequest {
        let (input, output) = match backend {
            AudioBackend::Asio => (None, None),
            AudioBackend::Wasapi => (self.wasapi_in.clone(), self.wasapi_out.clone()),
        };
        let buffer = buffer.filter(|_| backend.is_asio());
        DeviceRequest { backend, input, output, input_channel: Some(self.channel), buffer }
    }

    /// The switches' round: every other ASIO buffer and WASAPI, then back to `start`.
    fn cycle(&self, start: &DeviceRequest, asio: bool) -> Vec<DeviceRequest> {
        let mut all: Vec<DeviceRequest> =
            if asio { [256, 64, 128].into_iter().map(|b| self.request(AudioBackend::Asio, Some(b))).collect() } else { Vec::new() };
        all.push(self.request(AudioBackend::Wasapi, None));
        all.retain(|r| r != start);
        all.push(start.clone());
        all
    }
}

fn label(r: &DeviceRequest) -> String {
    match (r.backend, r.buffer) {
        (AudioBackend::Asio, Some(b)) => format!("asio {b}"),
        (AudioBackend::Asio, None) => "asio default".to_string(),
        (AudioBackend::Wasapi, _) => "wasapi".to_string(),
    }
}

fn describe(s: &DeviceStatus) -> String {
    format!(
        "{:?} {} Hz, block {}, in '{}', out '{}', align {} frames (input {})",
        s.backend, s.sample_rate, s.block, s.input_name, s.output_name, s.align_frames, s.input_frames
    )
}

/// Every counter that stays 0 in a clean run.
fn faults(d: &IoDiag) -> [(&'static str, u64); 18] {
    [
        ("gaps", d.gaps),
        ("xruns", d.xruns),
        ("lock_misses", d.lock_misses),
        ("duplex_faults", d.duplex_faults),
        ("join_starves", d.join_starves),
        ("join_overruns", d.join_overruns),
        ("join_trims", d.join_trims),
        ("share_starves", d.share_starves),
        ("share_overruns", d.share_overruns),
        ("share_trims", d.share_trims),
        ("commands_full", d.commands_full),
        ("panics", d.panics),
        ("rt_allocs", d.rt_allocs),
        ("engine.events_dropped", d.engine.events_dropped),
        ("engine.commands_dropped", d.engine.commands_dropped),
        ("engine.xruns", d.engine.xruns),
        ("engine.slot_events_dropped", d.engine.slot_events_dropped),
        ("engine.slot_protocol_errors", d.engine.slot_protocol_errors),
    ]
}

/// The counters that moved from `before` to `now`, or "all 0".
fn moved(now: &IoDiag, before: &IoDiag) -> String {
    let moved: Vec<String> = faults(now)
        .iter()
        .zip(faults(before).iter())
        .filter(|(n, b)| n.1 > b.1)
        .map(|(n, b)| format!("{}={}", n.0, n.1 - b.1))
        .collect();
    if moved.is_empty() { "all 0".to_string() } else { moved.join(" ") }
}

fn load_text(load: &BlockLoad) -> String {
    let bound = |k: usize| if k == LOAD_BINS - 1 { format!(">={k}%") } else { format!("<{}%", k + 1) };
    match (load.quantile(0.5), load.quantile(0.999), load.max()) {
        (Some(p50), Some(p999), Some(max)) => format!("p50{} p99.9{} max{}", bound(p50), bound(p999), bound(max)),
        _ => "none".to_string(),
    }
}

/// Where a phase started.
struct Mark {
    diag: IoDiag,
    load: BlockLoad,
    at: Instant,
}

struct Probe {
    host: EngineHost,
    plugins: Vec<PluginSpec>,
    slots: Vec<Slot>,
    events: Vec<Event>,
    /// Lane 0 as the engine last reported it (`None`: a new engine that has reported nothing yet).
    lane: Option<LaneInfo>,
    length: Frame,
    beats: u64,
    rate: u32,
    mute: bool,
    fails: Vec<(&'static str, String)>,
}

impl Probe {
    fn fail(&mut self, check: &'static str, message: String) {
        say(format!("! {check}: {message}"));
        self.fails.push((check, message));
    }

    fn send(&self, command: Command) -> Result<(), String> {
        self.host.send(TimedCommand { frame: None, command })
    }

    /// `--mute`: silence a new engine's master before anything sounds.
    fn silence(&self) -> Result<(), String> {
        if self.mute { self.send(Command::SetMasterMute(true)) } else { Ok(()) }
    }

    fn lane_is(&self, want: impl Fn(&LaneInfo) -> bool) -> bool {
        self.lane.as_ref().is_some_and(want)
    }

    /// Read the engine's events and the device's.
    fn pump(&mut self) {
        let mut events = std::mem::take(&mut self.events);
        self.host.drain_events(&mut events);
        for event in events.drain(..) {
            match event {
                Event::Lane { lane: 0, info, .. } => self.lane = Some(info),
                Event::Beat { .. } => self.beats += 1,
                Event::TakeRejected { lane, overdub, .. } => {
                    self.fail("loop", format!("lane {lane}: the {} saw an input gap and was discarded", if overdub { "overdub" } else { "take" }))
                }
                Event::PassDropped { lane, pass, .. } => self.fail("loop", format!("lane {lane}: RETAKE pass {pass} saw an input gap")),
                Event::Refused { lane, reason, .. } => say(format!("lane {lane} refused a press: {reason:?}")),
                _ => {}
            }
        }
        self.events = events;
        for event in self.host.take_device_events() {
            self.fail("events", format!("{event:?}"));
        }
    }

    fn wait(&mut self, what: &str, timeout: Duration, done: impl Fn(&Probe) -> bool) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        loop {
            self.pump();
            if done(self) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!("timed out after {} s: {what}", timeout.as_secs()));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn hold(&mut self, time: Duration) {
        let end = Instant::now() + time;
        while Instant::now() < end {
            self.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn mark(&self) -> Mark {
        Mark { diag: self.host.diag(), load: self.host.block_load(), at: Instant::now() }
    }

    /// Print a phase's counters and block load since `since`; its load.
    fn phase(&mut self, name: &str, since: &Mark) -> BlockLoad {
        self.pump();
        let (diag, load) = (self.host.diag(), self.host.block_load().since(&since.load));
        say(format!(
            "phase {name}: {:.1} s, callbacks {}, block {}, counters {}",
            since.at.elapsed().as_secs_f64(),
            diag.callbacks - since.diag.callbacks,
            load_text(&load),
            moved(&diag, &since.diag)
        ));
        load
    }

    fn load_slot(&mut self, k: usize, plugin: usize) {
        let (index, spec) = (self.slots[k].slot, self.plugins[plugin].clone());
        let sink: EventSink = Arc::new(move |event: EngineSlotEvent| say(format!("slot {index}: {event:?}")));
        let began = Instant::now();
        match engine_slot::load(spec.format, spec.path.clone(), spec.id.clone(), self.host.slot(index), 0, sink) {
            Ok(handle) => {
                say(format!("slot {index}: {} ({:?}) loaded in {} ms", handle.name(), handle.kind(), began.elapsed().as_millis()));
                self.slots[k].handle = Some(handle);
                self.slots[k].plugin = plugin;
            }
            Err(e) => self.fail("slots", format!("slot {index}: {} did not load: {e}", spec.name)),
        }
    }

    fn unload_slot(&mut self, k: usize) {
        let Some(handle) = self.slots[k].handle.take() else { return };
        let began = Instant::now();
        match handle.unload() {
            Ok(()) => say(format!("slot {}: unloaded in {} ms", self.slots[k].slot, began.elapsed().as_millis())),
            Err(e) => self.fail("slots", format!("slot {}: unload: {e}", self.slots[k].slot)),
        }
    }

    /// Every loaded plugin is in its slot again (a switch to another rate evicts them to be re-activated).
    fn expect_slots(&mut self, after: &str) {
        let loaded: Vec<usize> = self.slots.iter().filter(|s| s.handle.is_some()).map(|s| s.slot).collect();
        let core = self.host.core.clone();
        if let Err(e) = self.wait("the plugins are back in their slots", SLOT_WAIT, |_| loaded.iter().all(|&k| core.holder[k].load(Acquire) != 0)) {
            self.fail("slots", format!("after {after}: {e}"));
        }
    }

    /// The transport runs and the loop plays with its length, over `HOLD`.
    fn expect_loop(&mut self, after: &str) {
        let beats = self.beats;
        self.hold(HOLD);
        if self.beats == beats {
            self.fail("loop", format!("after {after}: no beat in {} s", HOLD.as_secs()));
        }
        let length = self.length;
        if !self.lane_is(|i| i.state == LaneState::Playing && i.length == length) {
            self.fail("loop", format!("after {after}: lane 0 is {:?}, not playing its {length} frames", self.lane));
        }
    }

    /// Record a one-bar loop of the input on lane 0 at 240 BPM (a bar is a second) through slot 0, live
    /// for the take only, and wait until it plays.
    fn record_loop(&mut self) -> Result<(), String> {
        self.send(Command::SetBpm(240.0))?;
        self.send(Command::SetSlotLive(0, true))?;
        self.send(Command::RecDub(0))?;
        self.wait("the take starts after the count-in", WAIT, |p| p.lane_is(|i| i.state == LaneState::Recording && !i.armed))?;
        self.hold(Duration::from_millis(1500));
        self.send(Command::RecDub(0))?;
        self.wait("the loop plays", WAIT, |p| p.lane_is(|i| i.state == LaneState::Playing && i.length > 0))?;
        self.send(Command::SetSlotLive(0, false))?;
        self.length = self.lane.map_or(0, |i| i.length);
        say(format!("loop on lane 0: {} frames ({:.2} s)", self.length, self.length as f64 / self.rate.max(1) as f64));
        Ok(())
    }

    fn soak(&mut self, seconds: f64) {
        let began = Instant::now();
        let end = began + Duration::from_secs_f64(seconds);
        let (start, mut note) = (self.host.diag(), began + Duration::from_secs(60));
        while Instant::now() < end {
            self.pump();
            if Instant::now() >= note {
                let diag = self.host.diag();
                say(format!("soak {:.0} s: callbacks {}, counters {}", began.elapsed().as_secs_f64(), diag.callbacks - start.callbacks, moved(&diag, &start)));
                note += Duration::from_secs(60);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn switch(&mut self, next: &DeviceRequest) -> Result<(), String> {
        let mark = self.mark();
        let began = Instant::now();
        match self.host.open(next.clone()) {
            Ok(status) => {
                let rebuilt = status.sample_rate != self.rate;
                say(format!(
                    "switch to {} in {} ms: {}{}",
                    label(next),
                    began.elapsed().as_millis(),
                    describe(&status),
                    if rebuilt { " (another rate: a new engine, and the loop went with the old one)" } else { "" }
                ));
                self.rate = status.sample_rate;
                self.expect_slots(&format!("the switch to {}", label(next)));
                if rebuilt {
                    self.lane = None;
                    self.silence()?;
                    self.record_loop()?;
                }
            }
            Err(e) => self.fail("switch", format!("to {}: {e}", label(next))),
        }
        self.expect_loop(&format!("the switch to {}", label(next)));
        self.phase(&format!("switch to {}", label(next)), &mark);
        Ok(())
    }

    /// Put the next plugin of the list into slot `k` while the loop plays.
    fn swap(&mut self, k: usize) {
        let mark = self.mark();
        let next = (self.slots[k].plugin + 1) % self.plugins.len();
        self.unload_slot(k);
        self.load_slot(k, next);
        let slot = self.slots[k].slot;
        self.expect_loop(&format!("the swap in slot {slot}"));
        self.phase(&format!("swap slot {slot}"), &mark);
    }

    fn drive(&mut self, a: &Args, devices: &Devices, start: &DeviceRequest) -> Result<(), String> {
        let mark = self.mark();
        let status = self.host.open(start.clone())?;
        self.silence()?;
        self.rate = status.sample_rate;
        say(format!("open {}: {}", label(start), describe(&status)));
        for k in 0..self.slots.len() {
            let plugin = self.slots[k].plugin;
            self.load_slot(k, plugin);
        }
        self.phase("open", &mark);

        let mark = self.mark();
        self.record_loop()?;
        self.phase("record", &mark);

        let mark = self.mark();
        self.soak(a.seconds);
        let load = self.phase("soak", &mark);
        match (load.quantile(0.999), load.max()) {
            (Some(p999), Some(max)) if p999 < P999_BAR && max < MAX_BAR => {}
            (Some(_), Some(_)) => self.fail("load", format!("soak block {}", load_text(&load))),
            _ => self.fail("load", "no callback in the soak".to_string()),
        }
        self.expect_loop("the soak");

        let cycle = devices.cycle(start, asio_available());
        if a.switches > 0 && cycle.len() == 1 {
            say("no other device to switch to (no ASIO driver cached): every switch reopens the same one");
        }
        for i in 0..a.switches {
            self.switch(&cycle[i % cycle.len()])?;
        }
        for i in 0..a.swaps {
            self.swap(i % self.slots.len());
        }
        Ok(())
    }
}

fn asio_available() -> bool {
    #[cfg(feature = "asio")]
    {
        crate::audio_output::asio_cache().is_some()
    }
    #[cfg(not(feature = "asio"))]
    {
        false
    }
}

/// The lag phase's hook in the output callback (`Rt::lag`): it adds the chirp to one output side every
/// `period` frames from `emit_from`, and records the engine's input, both counted from the first frame
/// it renders on the callback's frame counter. Preallocated: the callback never allocates in it.
pub(crate) struct LagRig {
    chirp: [f32; CHIRP_LEN],
    right: bool,
    period: usize,
    emit_from: usize,
    /// Off for `--split out`: a WASAPI client plays the chirps.
    emit: bool,
    start: Option<Frame>,
    recorded: Vec<f32>,
    len: usize,
    /// `--split`: each slice's first frame and when the callback ran it, while the capacity lasts.
    stamps: Vec<(usize, Instant)>,
}

impl LagRig {
    fn new(rate: u32, seconds: f64, right: bool, period: usize, split: Option<Split>) -> LagRig {
        let frames = ((seconds + 3.0) * rate as f64) as usize;
        LagRig {
            chirp: chirp(rate),
            right,
            period,
            emit_from: rate as usize,
            emit: split != Some(Split::Out),
            start: None,
            recorded: vec![0.0; frames],
            len: 0,
            stamps: Vec::with_capacity(if split.is_some() { frames / 16 } else { 0 }),
        }
    }

    /// One slice of the output callback: `input` the engine's input from device frame `frame`, `left`
    /// and `right` what it rendered there.
    pub(crate) fn block(&mut self, frame: Frame, input: &[f32], left: &mut [f32], right: &mut [f32]) {
        let start = *self.start.get_or_insert(frame);
        let Ok(off) = usize::try_from(frame - start) else { return };
        if self.stamps.len() < self.stamps.capacity() {
            self.stamps.push((off, Instant::now()));
        }
        let out = if self.right { right } else { left };
        for (k, (&x, y)) in input.iter().zip(out.iter_mut()).enumerate() {
            let f = off + k;
            if let Some(r) = self.recorded.get_mut(f) {
                *r = x;
                self.len = self.len.max(f + 1);
            }
            if self.emit && f >= self.emit_from && (f - self.emit_from) % self.period < CHIRP_LEN {
                *y += self.chirp[(f - self.emit_from) % self.period];
            }
        }
    }

    /// Each chirp found, (output frame, lag: input arrival minus that frame, sub-frame), and how many
    /// were not. A chirp's window runs to the next one's emission, so a lag up to about the period is
    /// found, and the first chirp (silence before it) finds none past it rather than an earlier chirp.
    fn lags(&self) -> (Vec<(usize, f64)>, usize) {
        let window = self.period - 2 * CHIRP_LEN;
        let (mut lags, mut invalid) = (Vec::new(), 0);
        let mut e = self.emit_from;
        while e + window < self.len {
            match find_arrivals(&self.recorded[..self.len], &self.chirp, e, e + window, false).0 {
                Some(hit) if hit.ncc >= 0.8 => lags.push((e, hit.pos - e as f64)),
                _ => invalid += 1,
            }
            e += self.period;
        }
        (lags, invalid)
    }
}

/// `--lag`: open the device with the chirp rig in the callback, play `--seconds`, close, and judge the
/// median lag against the alignment the engine rendered with (the Stage 1 A2 bar: within 1 ms).
fn lag_run(a: &Args, devices: &Devices, request: DeviceRequest) -> Result<(), String> {
    let rate = match request.backend {
        #[cfg(feature = "asio")]
        AudioBackend::Asio => crate::audio_output::asio_cache().map_or(48_000, |c| c.out_cfg.sample_rate),
        _ => 48_000,
    };
    // WASAPI (and a split, which crosses into it): a chirp about a second. Its round trip on the rig runs
    // past a quarter second (Stage 1's W1), and a lag past the period would alias onto the chirp before.
    let period = if request.backend.is_asio() && a.split.is_none() { rate as usize / 4 } else { rate as usize };
    if !request.backend.is_asio() {
        match wasapi_clocks::report(&request) {
            Ok(report) => say(report.to_string()),
            Err(e) => say(format!("wasapi clocks: {e}")),
        }
    }
    let host = EngineHost::with_driver(HostConfig::default(), super::cpal_driver::CpalDriver { preopen: a.preopen });
    host.core.rt.lock().map_err(|_| "engine lock poisoned")?.lag = Some(Box::new(LagRig::new(rate, a.seconds, a.out == 1, period, a.split)));
    let t0 = Instant::now();
    let began = Instant::now();
    let opened = host.open(request.clone());
    let open_ms = began.elapsed().as_millis();
    let status = match opened {
        Ok(status) => status,
        Err(e) => {
            host.shutdown();
            return Err(format!("open {}: {e}", label(&request)));
        }
    };
    say(format!("open {} in {open_ms} ms (preopen {}): {}", label(&request), if a.preopen { "on" } else { "off" }, describe(&status)));
    let wasapi = a.split.map(|split| {
        let (seconds, channel, (input, output)) = (a.seconds, if split == Split::Out { a.out } else { a.input as usize }, (devices.wasapi_in.clone(), devices.wasapi_out.clone()));
        std::thread::spawn(move || match split {
            Split::Out => wasapi_clocks::render_chirps(output, channel, seconds, period, t0),
            Split::In => wasapi_clocks::capture_record(input, channel, seconds, t0),
        })
    });
    std::thread::sleep(Duration::from_secs_f64(a.seconds + 1.0));
    let wasapi = wasapi.map(|t| t.join().unwrap_or_else(|_| Err("the WASAPI thread panicked".to_string())));
    let status = host.status().unwrap_or(status);
    let close = host.close();
    let rig = host.core.rt.lock().map_err(|_| "engine lock poisoned")?.lag.take();
    let diag = host.diag();
    host.shutdown();
    close?;
    let rig = rig.ok_or("the lag rig is gone")?;
    if let Some(side) = wasapi {
        return split_report(&rig, &status, side?, t0);
    }
    let (found, invalid) = rig.lags();
    let lags: Vec<f64> = found.iter().map(|&(_, lag)| lag).collect();
    let minutes: Vec<f64> = found.iter().map(|&(e, _)| e as f64 / status.sample_rate as f64 / 60.0).collect();
    let expected = status.align_frames as f64;
    let (lag, spread_f) = (median(&lags), spread(&lags));
    let ms = |frames: f64| frames * 1000.0 / status.sample_rate as f64;
    say(serde_json::json!({
        "phase": "lag", "backend": format!("{:?}", status.backend), "block": status.block, "rate": status.sample_rate,
        "preopen": a.preopen, "openMs": open_ms, "in": a.input, "out": a.out,
        "alignFrames": status.align_frames, "inputFrames": status.input_frames, "periodFrames": rig.period,
        "found": lags.len(), "invalid": invalid, "lagMedianFrames": lag, "lagSpreadFrames": spread_f,
        "lagFirstFrames": lags.first(), "lagLastFrames": lags.last(), "driftFramesPerMin": slope(&minutes, &lags),
        "residualFrames": lag - expected, "residualMs": ms(lag - expected), "counters": moved(&diag, &IoDiag::default()),
    })
    .to_string());
    if found.first().is_some_and(|&(e, _)| e != rig.emit_from) {
        say("! the first chirp found no arrival: a lag past the period lands on the next chirp's window, so these lags may be aliased");
    }
    // Each chirp's lag in order, to show a step or a drift inside the run.
    if lags.len() <= 64 {
        say(format!("lags {}", lags.iter().map(|l| format!("{l:.1}")).collect::<Vec<_>>().join(" ")));
    }
    for line in super::callback::trace::lines() {
        say(format!("trace {line}"));
    }
    if lags.is_empty() || invalid * 10 > lags.len() + invalid {
        say(format!("INVALID {invalid} of {} chirps below xcorr 0.8: check the cable and --in/--out", lags.len() + invalid));
        return Err("invalid run".into());
    }
    let pass = ms(lag - expected).abs() <= 1.0;
    say(format!(
        "{} A2 {:+.3}ms (lag {lag:.2}f, align {expected:.0}f, spread {spread_f:.2}f) | |median lag - (inLat+outLat)| <= 1.0 ms",
        if pass { "PASS" } else { "FAIL" },
        ms(lag - expected)
    ));
    if pass { Ok(()) } else { Err("the take would land off the grid".into()) }
}

/// What a `--split` WASAPI thread brings back, instants in seconds since the run's `t0`: the chirps it
/// played, each at (the instant cpal's playback stamp gives it: its padding and stream latency after the
/// callback; the instant the render clock's position reached it), or what it captured (the channel's
/// samples, and per packet (its first frame, the capture instant WASAPI stamped on it)).
enum WasapiSide {
    Played { chirps: Vec<(f64, f64)>, info: serde_json::Value },
    Captured { samples: Vec<f32>, packets: Vec<(f64, f64)>, rate: f64, info: serde_json::Value },
}

/// Seconds from `t0` to `t`, negative before it.
fn secs(t: Instant, t0: Instant) -> f64 {
    if t >= t0 { (t - t0).as_secs_f64() } else { -(t0 - t).as_secs_f64() }
}

/// On a track of (frame, seconds) marks at `rate`: the instant of `frame`, from the last mark at or
/// before it.
fn time_at(marks: &[(f64, f64)], frame: f64, rate: f64) -> Option<f64> {
    let i = marks.partition_point(|m| m.0 <= frame).checked_sub(1)?;
    Some(marks[i].1 + (frame - marks[i].0) / rate)
}

/// The frame at instant `t`, from the last mark at or before it.
fn frame_at(marks: &[(f64, f64)], t: f64, rate: f64) -> Option<f64> {
    let i = marks.partition_point(|m| m.1 <= t).checked_sub(1)?;
    Some(marks[i].0 + (t - marks[i].1) * rate)
}

/// `--split`: each chirp's arrival against the instant WASAPI's stamps put it at, in ms. ASIO's side
/// sits on its callbacks' instants: an input frame reached the converter its reported input latency
/// before its slice ran, an output frame leaves it the output latency after (A2 holds their sum).
fn split_report(rig: &LagRig, status: &DeviceStatus, side: WasapiSide, t0: Instant) -> Result<(), String> {
    let rate = status.sample_rate as f64;
    let (in_lat, out_lat) = (status.input_frames as f64, (status.align_frames - status.input_frames) as f64);
    let recorded = &rig.recorded[..rig.len];
    let stats = |xs: &[f64]| serde_json::json!({ "n": xs.len(), "median": median(xs), "spread": spread(xs) });
    // The search around a predicted arrival: a little before it, and most of a period after.
    let find = |sig: &[f32], at: f64, rate: f64| {
        let from = (at - 0.05 * rate).max(0.0) as usize;
        find_arrivals(sig, &rig.chirp, from, from + (0.75 * rate) as usize, false).0.filter(|h| h.ncc >= 0.8)
    };
    match side {
        WasapiSide::Played { chirps, info } => {
            let adc: Vec<(f64, f64)> = rig.stamps.iter().map(|&(f, t)| (f as f64, secs(t, t0) - in_lat / rate)).collect();
            let (mut after_stamp, mut after_clock, mut invalid) = (Vec::new(), Vec::new(), 0);
            for (stamp, clock) in chirps {
                let arrival = frame_at(&adc, stamp, rate).and_then(|at| find(recorded, at, rate)).and_then(|h| time_at(&adc, h.pos, rate));
                match arrival {
                    Some(t) => {
                        after_stamp.push((t - stamp) * 1000.0);
                        if clock.is_finite() {
                            after_clock.push((t - clock) * 1000.0);
                        }
                    }
                    None => invalid += 1,
                }
            }
            say(serde_json::json!({
                "phase": "split-out", "wasapi": info, "asioBlock": status.block, "asioIn": in_lat, "asioOut": out_lat, "invalid": invalid,
                "cableAfterPlaybackStampMs": stats(&after_stamp), "cableAfterRenderClockMs": stats(&after_clock),
            })
            .to_string());
        }
        WasapiSide::Captured { samples, packets, rate: wasapi_rate, info } => {
            let dac: Vec<(f64, f64)> = rig.stamps.iter().map(|&(f, t)| (f as f64, secs(t, t0) + out_lat / rate)).collect();
            let (mut stamp_after, mut invalid) = (Vec::new(), 0);
            let mut e = rig.emit_from;
            while e + rig.period <= rig.len {
                let left = time_at(&dac, e as f64, rate);
                let stamped = left
                    .and_then(|t| frame_at(&packets, t, wasapi_rate))
                    .and_then(|at| find(&samples, at, wasapi_rate))
                    .and_then(|h| time_at(&packets, h.pos, wasapi_rate));
                match (left, stamped) {
                    (Some(left), Some(stamped)) => stamp_after.push((stamped - left) * 1000.0),
                    _ => invalid += 1,
                }
                e += rig.period;
            }
            say(serde_json::json!({
                "phase": "split-in", "wasapi": info, "asioBlock": status.block, "asioIn": in_lat, "asioOut": out_lat, "invalid": invalid,
                "captureStampAfterCableMs": stats(&stamp_after),
            })
            .to_string());
        }
    }
    Ok(())
}

/// `--lag` on WASAPI: what WASAPI itself reports about the run's two endpoints, from a client of each
/// opened before the engine's streams (shared mode at the mix format, 2 s each, silent). Each stream's
/// reported latency (cpal adds the render one to its playback stamp and leaves the capture one out); on
/// the render side the frames the audio engine took from the buffer that its clock has not played yet
/// (a delay the render clock knows and the padding does not shows there); on the capture side each
/// packet's age (cpal's input latency) and how far the capture clock runs past the packets delivered.
mod wasapi_clocks {
    use std::time::{Duration, Instant};

    use serde_json::{json, Value};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, RPC_E_CHANGED_MODE};
    use windows::Win32::Media::Audio::{
        IAudioCaptureClient, IAudioClient, IAudioClock, IAudioRenderClient, IMMDevice, AUDCLNT_BUFFERFLAGS_SILENT,
        AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    };
    use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED};
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

    use super::super::DeviceRequest;
    use super::WasapiSide;
    use crate::chirp_lag::{chirp, median, CHIRP_LEN};

    const RUN: Duration = Duration::from_secs(2);

    pub(super) fn report(request: &DeviceRequest) -> Result<Value, String> {
        with_com(|| {
            let output = immdevice(&crate::audio_output::pick_output_device(request.output.as_deref())?)?;
            let input = immdevice(&crate::audio_input::pick_input_device(request.input.as_deref())?)?;
            Ok(json!({ "phase": "wasapi-clocks", "render": render(&output)?, "capture": capture(&input)? }))
        })
    }

    /// Run `body` with COM up on this thread; every COM object it made has dropped when it returns.
    fn with_com<T>(body: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
        // SAFETY: plain COM init on this thread, balanced below.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_err() && hr != RPC_E_CHANGED_MODE {
            return Err(format!("CoInitializeEx: {hr:?}"));
        }
        let result = body();
        if hr.is_ok() {
            // SAFETY: balances the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
        result
    }

    /// `--split out`: chirps on the render endpoint's `channel`, one each `period` frames from `period`,
    /// for `seconds`, at the stream's mix format (f32).
    pub(super) fn render_chirps(endpoint: Option<String>, channel: usize, seconds: f64, period: usize, t0: Instant) -> Result<WasapiSide, String> {
        with_com(|| {
            let device = immdevice(&crate::audio_output::pick_output_device(endpoint.as_deref())?)?;
            let (client, rate, channels, mut info, event) = open(&device)?;
            let chirp = chirp(rate as u32);
            // SAFETY: GetBuffer's frames are written in full and released before the next call; the
            // event outlives the loop.
            let played = unsafe {
                (|| -> Result<(Vec<(usize, f64)>, Vec<(f64, f64)>), String> {
                    let render: IAudioRenderClient = client.GetService().map_err(|e| format!("GetService(render): {e}"))?;
                    let clock: IAudioClock = client.GetService().map_err(|e| format!("GetService(clock): {e}"))?;
                    let freq = clock.GetFrequency().map_err(|e| format!("GetFrequency: {e}"))? as f64;
                    let size = client.GetBufferSize().map_err(|e| format!("GetBufferSize: {e}"))?;
                    // (chirp frame, its playback stamp), (instant, frames the clock played).
                    let (mut chirps, mut clocked) = (Vec::new(), Vec::new());
                    let mut written = 0usize;
                    let mut write = |frames: u32, stamp: f64| -> Result<(), String> {
                        let ptr = render.GetBuffer(frames).map_err(|e| format!("GetBuffer: {e}"))? as *mut f32;
                        let data = std::slice::from_raw_parts_mut(ptr, frames as usize * channels);
                        for (k, frame) in data.chunks_exact_mut(channels).enumerate() {
                            frame.fill(0.0);
                            let f = written + k;
                            if f >= period && (f - period) % period < CHIRP_LEN && channel < channels {
                                frame[channel] = chirp[(f - period) % period];
                                if (f - period) % period == 0 {
                                    chirps.push((f, stamp + k as f64 / rate));
                                }
                            }
                        }
                        render.ReleaseBuffer(frames, 0).map_err(|e| format!("ReleaseBuffer: {e}"))?;
                        written += frames as usize;
                        Ok(())
                    };
                    write(size, f64::NAN)?;
                    client.Start().map_err(|e| format!("Start: {e}"))?;
                    let end = Instant::now() + Duration::from_secs_f64(seconds);
                    while Instant::now() < end {
                        WaitForSingleObject(event, 100);
                        let padding = client.GetCurrentPadding().map_err(|e| format!("GetCurrentPadding: {e}"))?;
                        let mut position = 0u64;
                        clock.GetPosition(&mut position, None).map_err(|e| format!("GetPosition: {e}"))?;
                        let now = super::secs(Instant::now(), t0);
                        clocked.push((now, position as f64 / freq * rate));
                        if padding < size {
                            // cpal's playback stamp: the padding plays first (its stream latency is 0 here).
                            write(size - padding, now + padding as f64 / rate)?;
                        }
                    }
                    let _ = client.Stop();
                    Ok((chirps, clocked))
                })()
            };
            // SAFETY: the stream is stopped (or never started); nothing waits on the event.
            let _ = unsafe { CloseHandle(event) };
            let (chirps, clocked) = played?;
            // The instant the clock's played count reached each chirp, between the two wakes around it.
            let chirps = chirps
                .into_iter()
                .map(|(f, stamp)| {
                    let f = f as f64;
                    let i = clocked.partition_point(|c| c.1 < f);
                    let clock = match (i.checked_sub(1).map(|j| clocked[j]), clocked.get(i)) {
                        (Some(a), Some(b)) if b.1 > a.1 => a.0 + (f - a.1) / (b.1 - a.1) * (b.0 - a.0),
                        _ => f64::NAN,
                    };
                    (stamp, clock)
                })
                .collect();
            info["channels"] = json!(channels);
            Ok(WasapiSide::Played { chirps, info })
        })
    }

    /// `--split in`: record the capture endpoint's `channel` for `seconds`, with each packet's capture
    /// instant as WASAPI stamps it (the QPC `GetBuffer` gives, placed on `Instant` by the clock's QPC read
    /// beside it: cpal's capture stamp).
    pub(super) fn capture_record(endpoint: Option<String>, channel: usize, seconds: f64, t0: Instant) -> Result<WasapiSide, String> {
        with_com(|| {
            let device = immdevice(&crate::audio_input::pick_input_device(endpoint.as_deref())?)?;
            let (client, rate, channels, mut info, event) = open(&device)?;
            // SAFETY: each packet is read in full and released before the next GetBuffer; the event
            // outlives the loop.
            let recorded = unsafe {
                (|| -> Result<(Vec<f32>, Vec<(f64, f64)>), String> {
                    let capture: IAudioCaptureClient = client.GetService().map_err(|e| format!("GetService(capture): {e}"))?;
                    let clock: IAudioClock = client.GetService().map_err(|e| format!("GetService(clock): {e}"))?;
                    let mut samples = Vec::with_capacity(((seconds + 1.0) * rate) as usize);
                    let mut packets = Vec::new();
                    client.Start().map_err(|e| format!("Start: {e}"))?;
                    let end = Instant::now() + Duration::from_secs_f64(seconds);
                    while Instant::now() < end {
                        WaitForSingleObject(event, 100);
                        while capture.GetNextPacketSize().map_err(|e| format!("GetNextPacketSize: {e}"))? > 0 {
                            let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
                            let (mut first, mut stamp) = (0u64, 0u64);
                            capture
                                .GetBuffer(&mut data, &mut frames, &mut flags, Some(&mut first), Some(&mut stamp))
                                .map_err(|e| format!("GetBuffer: {e}"))?;
                            let (mut position, mut qpc) = (0u64, 0u64);
                            let clocked = clock.GetPosition(&mut position, Some(&mut qpc));
                            let now = super::secs(Instant::now(), t0);
                            let at = samples.len() as f64;
                            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() || channel >= channels {
                                samples.extend(std::iter::repeat_n(0.0, frames as usize));
                            } else {
                                let pcm = std::slice::from_raw_parts(data as *const f32, frames as usize * channels);
                                samples.extend(pcm.chunks_exact(channels).map(|frame| frame[channel]));
                            }
                            capture.ReleaseBuffer(frames).map_err(|e| format!("ReleaseBuffer: {e}"))?;
                            clocked.map_err(|e| format!("GetPosition: {e}"))?;
                            // Both QPC values are in 100 ns units.
                            packets.push((at, now - (qpc as f64 - stamp as f64) / 1e7));
                        }
                    }
                    let _ = client.Stop();
                    Ok((samples, packets))
                })()
            };
            // SAFETY: the stream is stopped (or never started); nothing waits on the event.
            let _ = unsafe { CloseHandle(event) };
            let (samples, packets) = recorded?;
            info["channels"] = json!(channels);
            Ok(WasapiSide::Captured { samples, packets, rate, info })
        })
    }

    fn immdevice(device: &cpal::Device) -> Result<IMMDevice, String> {
        #[allow(unreachable_patterns)]
        match device.as_inner() {
            cpal::platform::DeviceInner::Wasapi(d) => d.immdevice().ok_or_else(|| "the endpoint is gone".to_string()),
            _ => Err("not a WASAPI device".to_string()),
        }
    }

    fn stats(xs: &[f64]) -> Value {
        let (lo, hi) = xs.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &v| (lo.min(v), hi.max(v)));
        json!({ "median": median(xs), "min": lo, "max": hi, "n": xs.len() })
    }

    /// A shared, event-driven client at the mix format (f32 samples): (client, rate, channels, what it
    /// reports, its event).
    fn open(device: &IMMDevice) -> Result<(IAudioClient, f64, usize, Value, HANDLE), String> {
        // SAFETY: COM is initialised on this thread; the mix format is freed after Initialize copied it.
        unsafe {
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(|e| format!("Activate: {e}"))?;
            let mix = client.GetMixFormat().map_err(|e| format!("GetMixFormat: {e}"))?;
            let (rate, channels, bits) = ((*mix).nSamplesPerSec as f64, (*mix).nChannels as usize, (*mix).wBitsPerSample);
            let init = client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, 0, 0, mix, None);
            CoTaskMemFree(Some(mix as *const _));
            init.map_err(|e| format!("Initialize: {e}"))?;
            if bits != 32 || channels == 0 {
                return Err(format!("the mix format is {channels} channels of {bits} bits, not f32"));
            }
            let latency = client.GetStreamLatency().map_err(|e| format!("GetStreamLatency: {e}"))?;
            let size = client.GetBufferSize().map_err(|e| format!("GetBufferSize: {e}"))?;
            let (mut period, mut min) = (0i64, 0i64);
            client.GetDevicePeriod(Some(&mut period), Some(&mut min)).map_err(|e| format!("GetDevicePeriod: {e}"))?;
            let event = CreateEventW(None, false, false, PCWSTR::null()).map_err(|e| format!("CreateEventW: {e}"))?;
            if let Err(e) = client.SetEventHandle(event) {
                let _ = CloseHandle(event);
                return Err(format!("SetEventHandle: {e}"));
            }
            let info = json!({
                "rate": rate, "streamLatencyMs": latency as f64 / 1e4, "bufferFrames": size,
                "periodMs": { "default": period as f64 / 1e4, "min": min as f64 / 1e4 },
            });
            Ok((client, rate, channels, info, event))
        }
    }

    fn render(device: &IMMDevice) -> Result<Value, String> {
        let (client, rate, _, mut info, event) = open(device)?;
        // SAFETY: GetBuffer's frames are released before the next call; the event outlives the loop.
        let held = unsafe {
            (|| -> Result<Vec<f64>, String> {
                let render: IAudioRenderClient = client.GetService().map_err(|e| format!("GetService(render): {e}"))?;
                let clock: IAudioClock = client.GetService().map_err(|e| format!("GetService(clock): {e}"))?;
                let freq = clock.GetFrequency().map_err(|e| format!("GetFrequency: {e}"))? as f64;
                let size = client.GetBufferSize().map_err(|e| format!("GetBufferSize: {e}"))?;
                let silent = AUDCLNT_BUFFERFLAGS_SILENT.0 as u32;
                let write = |frames: u32| -> Result<(), String> {
                    render.GetBuffer(frames).map_err(|e| format!("GetBuffer: {e}"))?;
                    render.ReleaseBuffer(frames, silent).map_err(|e| format!("ReleaseBuffer: {e}"))
                };
                write(size)?;
                let mut written = size as f64;
                let mut held = Vec::new();
                client.Start().map_err(|e| format!("Start: {e}"))?;
                let end = Instant::now() + RUN;
                while Instant::now() < end {
                    WaitForSingleObject(event, 100);
                    let padding = client.GetCurrentPadding().map_err(|e| format!("GetCurrentPadding: {e}"))?;
                    let mut position = 0u64;
                    clock.GetPosition(&mut position, None).map_err(|e| format!("GetPosition: {e}"))?;
                    // Taken by the audio engine (written less the padding) less played by the clock.
                    held.push(written - padding as f64 - position as f64 / freq * rate);
                    if padding < size {
                        write(size - padding)?;
                        written += (size - padding) as f64;
                    }
                }
                let _ = client.Stop();
                Ok(held)
            })()
        };
        // SAFETY: the stream is stopped (or never started); nothing waits on the event.
        let _ = unsafe { CloseHandle(event) };
        info["engineHeldFrames"] = stats(&held?);
        Ok(info)
    }

    fn capture(device: &IMMDevice) -> Result<Value, String> {
        let (client, rate, _, mut info, event) = open(device)?;
        // SAFETY: each packet is released before the next GetBuffer; the event outlives the loop.
        let packets = unsafe {
            (|| -> Result<(Vec<f64>, Vec<f64>), String> {
                let capture: IAudioCaptureClient = client.GetService().map_err(|e| format!("GetService(capture): {e}"))?;
                let clock: IAudioClock = client.GetService().map_err(|e| format!("GetService(clock): {e}"))?;
                let freq = clock.GetFrequency().map_err(|e| format!("GetFrequency: {e}"))? as f64;
                let (mut ages, mut ahead) = (Vec::new(), Vec::new());
                client.Start().map_err(|e| format!("Start: {e}"))?;
                let end = Instant::now() + RUN;
                while Instant::now() < end {
                    WaitForSingleObject(event, 100);
                    while capture.GetNextPacketSize().map_err(|e| format!("GetNextPacketSize: {e}"))? > 0 {
                        let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
                        let (mut first, mut stamp) = (0u64, 0u64);
                        capture
                            .GetBuffer(&mut data, &mut frames, &mut flags, Some(&mut first), Some(&mut stamp))
                            .map_err(|e| format!("GetBuffer: {e}"))?;
                        let (mut position, mut now) = (0u64, 0u64);
                        let clocked = clock.GetPosition(&mut position, Some(&mut now));
                        capture.ReleaseBuffer(frames).map_err(|e| format!("ReleaseBuffer: {e}"))?;
                        clocked.map_err(|e| format!("GetPosition: {e}"))?;
                        // Both QPC stamps are in 100 ns units.
                        ages.push((now as f64 - stamp as f64) / 1e4);
                        ahead.push(position as f64 / freq * rate - (first + frames as u64) as f64);
                    }
                }
                let _ = client.Stop();
                Ok((ages, ahead))
            })()
        };
        // SAFETY: the stream is stopped (or never started); nothing waits on the event.
        let _ = unsafe { CloseHandle(event) };
        let (ages, ahead) = packets?;
        info["packetAgeMs"] = stats(&ages);
        info["clockAheadFrames"] = stats(&ahead);
        Ok(info)
    }
}

pub(crate) fn run(args: &[String]) -> Result<(), String> {
    let a = parse_args(args)?;
    if log::set_logger(&LOG).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
    let mut plugins: Vec<PluginSpec> = Vec::new();
    for (_, path) in &a.plugins {
        let spec = PluginSpec::scan(path)?;
        say(format!("plugin '{}' ({:?}, id {}) from {path}", spec.name, spec.format, spec.id));
        plugins.push(spec);
    }
    let slots = a.plugins.iter().enumerate().map(|(k, (slot, _))| Slot { slot: *slot, plugin: k, handle: None }).collect();
    let devices = Devices::new(a.input, a.device.as_deref())?;
    let start = devices.request(a.backend, a.buffer);
    #[cfg(feature = "asio")]
    if start.backend.is_asio() || a.switches > 0 {
        // A standalone DEV process: probe explicitly, with a throwaway sentinel (no app data dir here).
        let sentinel = std::env::temp_dir().join("bleeploop-engine-probe-asio");
        let report = crate::audio_output::probe_asio_startup(&sentinel, true);
        say(format!("asio startup: {}", serde_json::to_string(&report).unwrap_or_default()));
    }
    if a.lag {
        return lag_run(&a, &devices, start);
    }

    let host = EngineHost::new(HostConfig::default());
    let mut p = Probe {
        host: host.clone(),
        plugins,
        slots,
        events: Vec::new(),
        lane: None,
        length: 0,
        beats: 0,
        rate: 0,
        mute: a.mute,
        fails: Vec::new(),
    };
    if let Err(e) = p.drive(&a, &devices, &start) {
        p.fail("run", e);
    }
    // The plugins leave the engine while the device still plays (crossfaded out), then it closes.
    for k in 0..p.slots.len() {
        p.unload_slot(k);
    }
    if let Err(e) = host.close() {
        p.fail("run", format!("close: {e}"));
    }
    p.pump();
    host.shutdown();

    let total = host.diag();
    for line in super::callback::trace::lines() {
        say(format!("trace {line}"));
    }
    say(format!("total: callbacks {}, counters {}", total.callbacks, moved(&total, &IoDiag::default())));
    if faults(&total).iter().any(|(_, n)| *n > 0) {
        p.fails.push(("counters", moved(&total, &IoDiag::default())));
    }
    let errors = LOG.errors.load(Relaxed);
    if errors > 0 {
        p.fails.push(("log", format!("{errors} error(s) logged")));
    }
    let mut failed = 0;
    for (check, bar) in CHECKS {
        let found: Vec<&str> = p.fails.iter().filter(|(c, _)| *c == check).map(|(_, m)| m.as_str()).collect();
        if found.is_empty() {
            say(format!("PASS {check} | {bar}"));
        } else {
            failed += 1;
            say(format!("FAIL {check} {} | {bar}", found.join("; ")));
        }
    }
    if failed > 0 {
        return Err(format!("{failed} check(s) failed"));
    }
    say("all checks pass");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rig whose input holds each chirp `lag` frames after it left.
    fn looped(period: usize, lag: usize) -> LagRig {
        let mut rig = LagRig::new(44_100, 5.0, true, period, None);
        let mut e = rig.emit_from;
        while e + lag + CHIRP_LEN <= rig.recorded.len() {
            rig.recorded[e + lag..e + lag + CHIRP_LEN].copy_from_slice(&rig.chirp);
            e += period;
        }
        rig.len = rig.recorded.len();
        rig
    }

    #[test]
    fn a_lag_up_to_the_period_is_found_without_aliasing() {
        // ASIO's quarter second; WASAPI's second holds the rig's ~264 ms round trip.
        for (period, lag) in [(11_025, 364), (44_100, 11_624), (44_100, 40_000)] {
            let (found, invalid) = looped(period, lag).lags();
            assert!(!found.is_empty() && invalid <= 1, "period {period}, lag {lag}: {} found, {invalid} not", found.len());
            assert!(found.iter().all(|&(_, l)| (l - lag as f64).abs() < 0.5), "period {period}, lag {lag}: {found:?}");
        }
        // Past the period, the first chirp finds nothing rather than the one before it.
        let (found, _) = looped(11_025, 12_000).lags();
        assert!(found.iter().all(|&(e, _)| e > 44_100), "{found:?}");
    }
}
