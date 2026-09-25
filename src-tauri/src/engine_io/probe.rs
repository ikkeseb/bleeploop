//! DEV engine probe (`docs/plans/native-engine.md` § Stage 4, the rig gates): the device side on real
//! hardware, headless. `app.exe --probe-engine …` exits before Tauri starts. It opens the device, loads
//! a plugin into each slot asked for, records a loop on lane 0, soaks, then switches backend and buffer
//! size and swaps the plugins while the loop plays, printing each phase's counters. It fails when a
//! counter moved, the device reported an event, the loop stopped at an unchanged rate, a plugin did not
//! come back, an error was logged, or the soak missed the block-load bar.
//!
//! `app.exe --probe-engine <asio|wasapi> <64|128|256|default> [--plugin <slot>=<file.vst3|.clap>]...
//! [--seconds N] [--switches N] [--swaps N] [--in N] [--device <WASAPI name substring>] [--mute]`
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
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    const USAGE: &str = "usage: --probe-engine <asio|wasapi> <64|128|256|default> [--plugin <slot>=<file.vst3|.clap>]... \
        [--seconds N] [--switches N] [--swaps N] [--in N] [--device <WASAPI name substring>] [--mute]";
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
    let mut parsed = Args { backend, buffer, plugins: Vec::new(), seconds: 60.0, switches: 0, swaps: 0, input: 0, device: None, mute: false };
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
            _ => return Err(format!("unknown argument {flag}\n{USAGE}")),
        }
    }
    if !(3.0..=3600.0).contains(&parsed.seconds) {
        return Err("the soak runs 3 s .. 60 min".into());
    }
    if parsed.swaps > 0 && parsed.plugins.is_empty() {
        return Err("--swaps needs a --plugin".into());
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
        if let Err(e) = self.wait("the plugins are back in their slots", SLOT_WAIT, |_| loaded.iter().all(|&k| core.occupied[k].load(Acquire))) {
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
