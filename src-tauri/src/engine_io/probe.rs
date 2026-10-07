//! DEV engine probe, the rig gate: the device side on real
//! hardware, headless. `app.exe --probe-engine …` exits before Tauri starts. It opens the device, loads
//! a plugin into each slot asked for, records a loop on lane 0, soaks, then switches backend and buffer
//! size and swaps the plugins while the loop plays, printing each phase's counters. It fails when a
//! counter moved, the device reported an event, the loop stopped at an unchanged rate, a plugin did not
//! come back, an error was logged, or the soak missed the block-load bar. A WASAPI open's first 3 s may
//! trim or starve the input join (D25): the counter check forgives those there (`forgivable`).
//!
//! The rig's clean run to compare against (ASIO 128, Archetype Petrucci X and Pro-Q 3, a 600 s soak,
//! 20 switches, 4 swaps): every counter 0 over ~225 000 callbacks, soak block time p99.9 < 33 % and
//! max < 52 % of the period, plugin loads 7–78 ms. Every ASIO re-open logs a BadMode input build and
//! its retry (`cpal_driver`'s `retry_on_asio`): expected, not a fault.
//!
//! `app.exe --probe-engine <asio|wasapi> <64|128|256|default> [--plugin <slot>=<file.vst3|.clap|.dll>]...
//! [--seconds N] [--switches N] [--swaps N] [--cycle <asio64|asio128|asio256|wasapi>,...] [--hold S]
//! [--pause MS] [--in N] [--device <WASAPI name substring>] [--mute] [--share <endpoint id>]
//! [--scene <default|heavy>] [--tone N [--out N] [--tone-from S] [--tone-level X]] [--lag [--out N] [--no-preopen] [--split <out|in>]]`
//!
//! `--scene heavy` loads the engine for the soak, set up once the loop plays and held through the soak
//! and every phase after it (`HEAVY_FX`, `HEAVY_SENDS`, `CHORD`): lane 0's loop copied to all five lanes,
//! every lane's five effects on, the three input sends on, every loaded slot live on input `--in`, a
//! four-note chord held on the Pad and the click on. Its soak is judged on the max alone: a loaded
//! scene's p99.9 sits higher by design, and the question it answers is the deadline. Its clean run
//! (ASIO 64, `--profile=rig`, both plugins, `--mute`, a 600 s soak, an idle machine, 2026-10-07):
//! every fault counter 0 over ~413 000 callbacks, block time p50 < 26 %, p99.9 < 53 %, max < 76 %,
//! none over budget.
//!
//! `--cycle` names the switches' round instead of the default one (every other ASIO buffer and WASAPI,
//! then back to the start): `--cycle wasapi` on a WASAPI run closes and reopens WASAPI at every switch
//! (an open of the device that runs is no reopen: the probe closes it first), the most WASAPI opens a
//! minute. After every WASAPI phase the probe prints the join's pushes and pulls since that open
//! (`callback.rs`'s trace). `--hold` is how long each switch plays before the probe looks (default 2 s);
//! `--pause` closes the device and waits that long before every switch to WASAPI (does the endpoints'
//! start depend on what ran just before).
//!
//! `--lag` runs only the lag phase instead (the Stage 1 A2 bar on the engine's own open path): a chirp
//! leaves on output `--out` (0-based, default 1) every quarter second (WASAPI: every second) and comes
//! back through a loopback cable on input `--in`; each arrival's lag is compared with the alignment the
//! engine renders with (the driver's input plus output latency). `--no-preopen` opens ASIO without the
//! preopen (`cpal_driver`), for a before-and-after comparison. A WASAPI lag run first prints what
//! WASAPI's own clocks report (`wasapi_clocks`).
//!
//! `--tone N` (ASIO, with `--mute`) plays a steady sine on output `--out` (0 or 1, default 1) through the
//! soak and reads it back through a loopback cable on input N, which slot 1 reads for the whole run
//! (slot 0 keeps `--in`, and N is not `--in`): does a callback that runs long or enters late leave a
//! discontinuity in it. The hook, the detector and what they cannot see: `tone.rs`. The tone is added
//! after the master fade, so the mute leaves it alone. It is calibrated before the soak, and two planted
//! controls (64 frames of silence in the output tone, one before the soak and one after it) must each
//! come back on the alignment. The closing block lists every event with the nearest long or late
//! callback, and the `tone` check ends one of three ways. `tone: clean` (PASS): both controls found, full
//! coverage, no event; it names the long callbacks and late entries the run had, since a clean tone over
//! none of them says nothing about them. `tone: discontinuities` (FAIL): the tone broke, with how many
//! events fall at a long or late callback. `tone: not measured` (FAIL): no tone, a clipped or unsteady
//! input, a control not found, a coverage gap, a full table, a device interrupted under the tone or a
//! callback log that did not record. Its limit: the cable returns through the same driver's input, so an
//! event is a loopback discontinuity, and may be the input's. The tone starts 10 s into the stream
//! (`TONE_FROM`), after the setup; `--tone-from S` starts it S seconds in, right after the open and so
//! before the plugins, the take and the scene, to watch the stream's first seconds, and `--tone-level X`
//! plays it at amplitude X (default 0.25). An event that opens with no residual spike gets a trace of its
//! windows, whose size against the level tells a gain or clock change from something added to the signal
//! (`tone.rs`, `Trace`).
//!
//! `--split out|in` (ASIO, with `--device`) splits the WASAPI round trip by its sides, against ASIO's
//! (whose report A2 holds) on QPC: `out` plays the chirps from a WASAPI render client of the `--device`
//! endpoint and ASIO records them; `in` plays them from ASIO and a WASAPI capture client records them.
//! Each arrival is compared with the instant WASAPI's own stamps put it at.
//!
//! The take records input channel `--in` (0-based, default 0) through slot 0, live for the take only
//! (the heavy scene keeps every loaded slot live): keep that channel off a loopback cable, or the
//! monitor feeds back through it. A swap puts the
//! next plugin of the `--plugin` list into a slot (with one, the same plugin again). `--mute` mutes the
//! master, the monitored input included: the device plays silence, so a WASAPI run can share the
//! interface with other apps. `--share` turns Share output on after the open, mirroring the master to
//! that WASAPI render endpoint (the id the release log names, `wasapi:{…}.{…}`) while ASIO plays, so the
//! soak counts the mirror's starves, overruns and trims too.

use std::sync::atomic::{AtomicU64, Ordering::{Acquire, Relaxed}};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lf_engine::dsp::fx::{FxKind, FxParam};
use lf_engine::grid::Frame;
use lf_engine::{Command, Event, InputSend, InputSendParam, Instrument, LaneInfo, LaneState, NoteTarget, TimedCommand, SLOT_COUNT, TRACK_COUNT};

use super::tone::{Facts, Shared, ToneRig};
use super::{BlockLoad, DeviceRequest, DeviceStatus, EngineHost, HostConfig, IoDiag};
use crate::chirp_lag::{chirp, find_arrivals, median, slope, spread, CHIRP_LEN};
use crate::audio_output::AudioBackend;
use crate::host::engine_slot::{self, EngineSlotEvent, EngineSlotHandle, EventSink, PluginFormat};

/// How long a swap plays before the probe looks again, and a switch without `--hold`.
const HOLD: Duration = Duration::from_secs(2);
/// How long a plugin may take to come back into its slot after a switch.
const SLOT_WAIT: Duration = Duration::from_secs(5);
const WAIT: Duration = Duration::from_secs(10);
/// `--tone` starts once the device frame counter has run this long (`--tone-from` moves it). On the
/// rig's interface (ASIO 64, 44.1 kHz) a signal added near the tone's frequency beats with the returning
/// tone about 6.8 s in, fading over some 0.6 s, with any scene, with and without plugins, no callback
/// long or late; past the thresholds in most runs that watched it, always at half level. Its source is
/// unknown (`src-tauri/AGENTS.md` § Open threads, the crackle thread); the tone run is about what comes
/// later.
const TONE_FROM: Duration = Duration::from_secs(10);
/// The soak's block-load bar, in whole percent of the period.
const P999_BAR: usize = 50;
const MAX_BAR: usize = 90;
/// How long from a WASAPI open's start the counter check forgives the join's trims and starves (D25:
/// the endpoints' start, `src-tauri/AGENTS.md` § Open threads).
const GRACE: Duration = Duration::from_secs(3);

/// The heavy scene's lane effects, the same on every lane: each processes (a nonzero mix, send, shift).
const HEAVY_FX: [(FxParam, f64); 8] = [
    (FxParam::Cutoff, 2000.0),
    (FxParam::Q, 4.0),
    (FxParam::Semitones, 7.0),
    // 1/16.
    (FxParam::Rate, 3.0),
    // 1/8 dotted.
    (FxParam::Time, 2.0),
    (FxParam::Feedback, 0.5),
    (FxParam::Mix, 0.4),
    (FxParam::Amount, 0.5),
];
/// The heavy scene's input sends.
const HEAVY_SENDS: [(InputSendParam, f64); 6] = [
    // 1/8.
    (InputSendParam::EchoTime, 1.0),
    (InputSendParam::EchoFeedback, 0.5),
    (InputSendParam::EchoLevel, 0.5),
    (InputSendParam::ReverbLevel, 0.5),
    (InputSendParam::RingFreq, 440.0),
    (InputSendParam::RingLevel, 0.3),
];
/// The heavy scene's chord on the Pad (C3 E3 G3 B3), the built-in synth with the most work a voice: an
/// FM pair whose carrier runs at an audio-rate frequency, with two envelopes, sustaining at 0.9.
const CHORD: [u8; 4] = [48, 52, 55, 59];

/// Every check, in report order, with its bar.
const CHECKS: [(&str, &str); 9] = [
    ("run", "the device opens and a loop records"),
    ("switch", "every backend and buffer switch starts"),
    ("slots", "every plugin loads, is back in its slot after each switch, reloads on a swap and unloads"),
    ("loop", "the loop plays through every phase at an unchanged rate; no take or pass rejected"),
    ("events", "no device event (loss, fallback, engine fault)"),
    ("counters", "every fault counter (IoDiag::faults) stays 0 (a WASAPI open's first 3 s may trim or starve the join)"),
    ("load", "soak: block time p99.9 < 50 % (not judged for the heavy scene), max < 90 % of the period"),
    ("log", "no error logged"),
    ("tone", "--tone: both controls come back on the alignment, full coverage, no loopback discontinuity in the returning tone (the cable returns through the same driver's input)"),
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
    /// `--cycle`: the switches' round, each a backend and its buffer.
    cycle: Vec<(AudioBackend, Option<u32>)>,
    hold: Duration,
    pause: Option<Duration>,
    input: u32,
    device: Option<String>,
    mute: bool,
    /// `--share`: Share output's endpoint, switched on after the open.
    share: Option<String>,
    /// `--scene heavy`.
    heavy: bool,
    /// `--tone`: the input the loopback tone returns on; `--tone-from`, when it starts in the stream;
    /// `--tone-level`, its amplitude.
    tone: Option<u32>,
    tone_from: Option<Duration>,
    tone_level: f32,
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
    const USAGE: &str = "usage: --probe-engine <asio|wasapi> <64|128|256|default> [--plugin <slot>=<file.vst3|.clap|.dll>]... \
        [--seconds N] [--switches N] [--swaps N] [--cycle <asio64|asio128|asio256|wasapi>,...] [--hold S] [--pause MS] [--in N] [--device <WASAPI name substring>] [--mute] [--share <endpoint id>] \
        [--scene <default|heavy>] [--tone N [--out N] [--tone-from S] [--tone-level X]] [--lag [--out N] [--no-preopen] [--split <out|in>]]";
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
        cycle: Vec::new(),
        hold: HOLD,
        pause: None,
        input: 0,
        device: None,
        mute: false,
        share: None,
        heavy: false,
        tone: None,
        tone_from: None,
        tone_level: super::tone::LEVEL,
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
            "--cycle" => {
                for item in value()?.split(',') {
                    parsed.cycle.push(match item {
                        "wasapi" => (AudioBackend::Wasapi, None),
                        "asio64" => (AudioBackend::Asio, Some(64)),
                        "asio128" => (AudioBackend::Asio, Some(128)),
                        "asio256" => (AudioBackend::Asio, Some(256)),
                        other => return Err(format!("--cycle {other}: expected asio64, asio128, asio256 or wasapi")),
                    });
                }
            }
            "--hold" => parsed.hold = Duration::try_from_secs_f64(number(value()?)?).map_err(|e| format!("--hold: {e}"))?,
            "--pause" => parsed.pause = Some(Duration::try_from_secs_f64(number(value()?)? / 1e3).map_err(|e| format!("--pause: {e}"))?),
            "--in" => parsed.input = number(value()?)? as u32,
            "--device" => parsed.device = Some(value()?),
            "--mute" => parsed.mute = true,
            "--share" => parsed.share = Some(value()?),
            "--scene" => {
                parsed.heavy = match value()?.as_str() {
                    "default" => false,
                    "heavy" => true,
                    other => return Err(format!("--scene {other}: expected default or heavy")),
                }
            }
            "--tone" => parsed.tone = Some(number(value()?)? as u32),
            "--tone-from" => {
                let seconds = number(value()?)?;
                if !(0.0..=60.0).contains(&seconds) {
                    return Err("--tone-from: expected 0 .. 60 s".into());
                }
                parsed.tone_from = Some(Duration::from_secs_f64(seconds));
            }
            "--tone-level" => {
                parsed.tone_level = number(value()?)? as f32;
                if !(0.01..=0.5).contains(&parsed.tone_level) {
                    return Err("--tone-level: expected 0.01 .. 0.5".into());
                }
            }
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
    if let Some(tone) = parsed.tone {
        if parsed.lag {
            return Err("--tone runs in the soak: not with --lag".into());
        }
        if !backend.is_asio() {
            return Err("--tone needs asio: WASAPI's join resamples the input, so the returning tone's phase is no constant".into());
        }
        if !parsed.mute {
            return Err("--tone needs --mute: the cable would feed a live slot back through the master".into());
        }
        if parsed.out > 1 {
            return Err("--tone plays on --out 0 or 1".into());
        }
        if tone == parsed.input {
            return Err("--tone and --in name the same input: the take records --in, the cable returns on --tone".into());
        }
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
        let format = PluginFormat::of_path(std::path::Path::new(path)).ok_or_else(|| format!("{path}: not a .clap, .vst3 or .dll"))?;
        let first = crate::host::scan_one(path)?.plugins.into_iter().next().ok_or_else(|| format!("{path}: no plugin inside"))?;
        Ok(PluginSpec { path: path.to_string(), format, id: first.id, name: first.name })
    }
}

struct Slot {
    slot: usize,
    /// Index into `Probe::plugins` of what the slot holds (or last held).
    plugin: usize,
    handle: Option<EngineSlotHandle>,
}

/// The requests the probe opens: WASAPI endpoints picked once by name, the capture channel fixed
/// (`--tone`: slot 1 reads the cable's input instead).
struct Devices {
    wasapi_in: Option<String>,
    wasapi_out: Option<String>,
    channel: u32,
    tone: Option<u32>,
}

impl Devices {
    fn new(channel: u32, tone: Option<u32>, name: Option<&str>) -> Result<Devices, String> {
        let Some(name) = name.map(str::to_lowercase) else {
            return Ok(Devices { wasapi_in: None, wasapi_out: None, channel, tone });
        };
        let input = crate::audio_input::list_input_devices()?.into_iter().find(|d| d.name.to_lowercase().contains(&name));
        let output = crate::audio_output::list_output_devices()?.into_iter().find(|d| d.name.to_lowercase().contains(&name));
        match (input, output) {
            (Some(i), Some(o)) => {
                say(format!("wasapi endpoints: in '{}', out '{}'", i.name, o.name));
                Ok(Devices { wasapi_in: Some(i.id), wasapi_out: Some(o.id), channel, tone })
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
        let input_channels = std::array::from_fn(|slot| Some(self.tone.filter(|_| slot == 1).unwrap_or(self.channel)));
        DeviceRequest { backend, input, output, input_channels, buffer, sample_rate: None }
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

/// The counters that moved from `before` to `now`, or "all 0".
fn moved(now: &IoDiag, before: &IoDiag) -> String {
    now.moved_since(before).unwrap_or_else(|| "all 0".to_string())
}

/// What a grace window forgives, as it moved from `before` to `now`: the join's trims and starves, and
/// the engine xruns that count the blocks they damaged. Every other cause of an engine xrun moves a
/// counter of its own (gaps, xruns, lock_misses, join_overruns), which no window forgives.
fn forgivable(now: &IoDiag, before: &IoDiag) -> IoDiag {
    IoDiag {
        join_trims: now.join_trims - before.join_trims,
        join_starves: now.join_starves - before.join_starves,
        engine: lf_engine::Diag { xruns: now.engine.xruns.saturating_sub(before.engine.xruns), ..Default::default() },
        ..Default::default()
    }
}

/// The run's counters less what the grace windows forgave: the counter check's.
fn unforgiven(total: &IoDiag, forgiven: &IoDiag) -> IoDiag {
    IoDiag {
        join_trims: total.join_trims.saturating_sub(forgiven.join_trims),
        join_starves: total.join_starves.saturating_sub(forgiven.join_starves),
        engine: lf_engine::Diag { xruns: total.engine.xruns.saturating_sub(forgiven.engine.xruns), ..total.engine },
        ..*total
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
    /// Each lane as the engine last reported it (`None`: a new engine that has reported nothing yet).
    lanes: [Option<LaneInfo>; TRACK_COUNT],
    length: Frame,
    beats: u64,
    rate: u32,
    mute: bool,
    /// `--scene heavy`.
    heavy: bool,
    /// `--hold` and `--pause`, and whether `--cycle` named the round.
    hold: Duration,
    pause: Option<Duration>,
    cycled: bool,
    /// The WASAPI open whose grace window runs: the counters just before it, and when the window ends.
    /// Anything that ends the device or stops the pump lets the window run out first (`finish_grace`).
    grace: Option<(IoDiag, Instant)>,
    /// What the grace windows forgave.
    forgiven: IoDiag,
    /// `--tone`: the tone run's shared state, whether the tone plays, and the run's facts for its report.
    tone: Option<Arc<Shared>>,
    /// `--tone-from`: the tone starts then, before the plugins, the take and the scene.
    tone_from: Option<Duration>,
    tone_plays: bool,
    tone_facts: Option<Facts>,
    /// The device the tone started on (backend, block, rate), and what interrupted it since.
    tone_device: Option<(AudioBackend, u32, u32)>,
    tone_interrupted: Option<String>,
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

    /// Lane 0 is as `want` says.
    fn lane_is(&self, want: impl Fn(&LaneInfo) -> bool) -> bool {
        self.lanes[0].as_ref().is_some_and(want)
    }

    /// Every lane plays a loop of `length` frames.
    fn all_play(&self, length: Frame) -> bool {
        self.lanes.iter().all(|l| l.is_some_and(|i| i.state == LaneState::Playing && i.length == length))
    }

    /// Read the engine's events and the device's.
    fn pump(&mut self) {
        let mut events = std::mem::take(&mut self.events);
        self.host.drain_events(&mut events);
        for event in events.drain(..) {
            match event {
                Event::Lane { lane, info, .. } if (lane as usize) < TRACK_COUNT => self.lanes[lane as usize] = Some(info),
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
            if self.tone_plays && self.tone_interrupted.is_none() {
                self.tone_interrupted = Some(format!("{event:?}"));
            }
            self.fail("events", format!("{event:?}"));
        }
        self.settle();
    }

    /// End the grace window once it has run its time, forgiving what it moved that a WASAPI open may.
    fn settle(&mut self) {
        let Some((before, until)) = self.grace else { return };
        if Instant::now() < until {
            return;
        }
        self.grace = None;
        let f = forgivable(&self.host.diag(), &before);
        if let Some(text) = f.moved_since(&IoDiag::default()) {
            say(format!("forgiven in the WASAPI open's first {} s: {text}", GRACE.as_secs()));
        }
        self.forgiven.join_trims += f.join_trims;
        self.forgiven.join_starves += f.join_starves;
        self.forgiven.engine.xruns += f.engine.xruns;
    }

    /// Let a running grace window run out before a switch or work that does not pump (a plugin load or
    /// unload can block for seconds): a settle then neither cuts the window short nor forgives past it.
    fn finish_grace(&mut self) {
        if let Some((_, until)) = self.grace {
            self.hold(until.saturating_duration_since(Instant::now()));
            self.settle();
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
            load.text(),
            moved(&diag, &since.diag)
        ));
        load
    }

    fn load_slot(&mut self, k: usize, plugin: usize) {
        self.finish_grace();
        let (index, spec) = (self.slots[k].slot, self.plugins[plugin].clone());
        let sink: EventSink = Arc::new(move |event: EngineSlotEvent| say(format!("slot {index}: {event:?}")));
        let began = Instant::now();
        match engine_slot::load(spec.format, spec.path.clone(), spec.id.clone(), self.host.slot(index), 0, sink, None) {
            Ok(handle) => {
                say(format!("slot {index}: {} ({:?}) loaded in {} ms", handle.name(), handle.kind(), began.elapsed().as_millis()));
                self.slots[k].handle = Some(handle);
                self.slots[k].plugin = plugin;
            }
            Err(e) => self.fail("slots", format!("slot {index}: {} did not load: {e}", spec.name)),
        }
    }

    fn unload_slot(&mut self, k: usize) {
        self.finish_grace();
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

    /// The transport runs and the loop plays with its length, over `time`.
    fn expect_loop(&mut self, after: &str, time: Duration) {
        let beats = self.beats;
        self.hold(time);
        if self.beats == beats {
            self.fail("loop", format!("after {after}: no beat in {} s", time.as_secs_f64()));
        }
        let length = self.length;
        if !self.lane_is(|i| i.state == LaneState::Playing && i.length == length) {
            self.fail("loop", format!("after {after}: lane 0 is {:?}, not playing its {length} frames", self.lanes[0]));
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
        self.length = self.lanes[0].map_or(0, |i| i.length);
        say(format!("loop on lane 0: {} frames ({:.2} s)", self.length, self.length as f64 / self.rate.max(1) as f64));
        Ok(())
    }

    /// `--scene heavy` (the header), once lane 0's loop plays: each copy lands in the first empty lane.
    fn heavy_scene(&mut self) -> Result<(), String> {
        for _ in 1..TRACK_COUNT {
            self.send(Command::Copy(0))?;
        }
        let length = self.length;
        self.wait("every lane plays the loop", WAIT, |p| p.all_play(length))?;
        for lane in 0..TRACK_COUNT as u8 {
            for (param, value) in HEAVY_FX {
                self.send(Command::SetFxParam(lane, param, value))?;
            }
            for kind in FxKind::ALL {
                self.send(Command::SetFxBypass(lane, kind, false))?;
            }
        }
        for send in InputSend::ALL {
            self.send(Command::SetInputSend(send, true))?;
        }
        for (param, value) in HEAVY_SENDS {
            self.send(Command::SetInputSendParam(param, value))?;
        }
        let mut live = Vec::new();
        for k in 0..SLOT_COUNT {
            let loaded = self.slots.iter().any(|s| s.slot == k && s.handle.is_some());
            self.send(Command::SetSlotLive(k as u8, loaded))?;
            if loaded {
                live.push(k.to_string());
            }
        }
        self.send(Command::SelectInstrument(NoteTarget::Builtin(Instrument::Pad)))?;
        for note in CHORD {
            self.send(Command::NoteOn(note, 0.6))?;
        }
        self.send(Command::SetMetronome(true))?;
        self.hold(HOLD);
        say(format!(
            "heavy scene: {TRACK_COUNT} lanes playing, every effect on each, 3 input sends, slots live [{}], Pad chord {CHORD:?}, click on",
            live.join(", ")
        ));
        Ok(())
    }

    /// `--tone`: start the tone, and wait for its calibration and for the first control to come back.
    fn tone_start(&mut self) -> Result<(), String> {
        let Some(shared) = self.tone.clone().filter(|_| !self.tone_plays) else { return Ok(()) };
        // Past the stream's first seconds (`TONE_FROM`, or `--tone-from`).
        let after = self.tone_from.unwrap_or(TONE_FROM);
        let from = (after.as_secs_f64() * self.rate as f64) as Frame;
        self.wait("the stream has run its first seconds", after + WAIT, |p| p.host.core.frame.load(Relaxed) >= from)?;
        let now = self.host.core.frame.load(Relaxed);
        say(format!("tone: starts {:.2} s into the stream (asked for {:.2} s)", now as f64 / self.rate as f64, after.as_secs_f64()));
        shared.begin(self.rate);
        self.tone_plays = true;
        self.wait("the tone calibrates", WAIT, |_| shared.cal() != super::tone::Cal::Pending)?;
        if let Some(fault) = shared.cal().fault() {
            return Err(format!("tone: {fault}"));
        }
        self.wait("the tone's first control comes back", WAIT, |_| shared.counts().0 > 0)?;
        // The control's event closes before the soak counts.
        self.hold(Duration::from_millis(300));
        let status = self.host.status().ok_or("tone: no device runs")?;
        self.tone_device = Some((status.backend, status.block, status.sample_rate));
        self.tone_facts = Some(Facts { align: status.align_frames, block: status.block, soak_from: self.host.core.frame.load(Relaxed), interrupted: None });
        say("tone: calibrated, and the first control came back");
        Ok(())
    }

    /// `--tone`: plant the second control and fade the tone out, before anything stops the device.
    fn tone_stop(&mut self) {
        let Some(shared) = self.tone.clone() else { return };
        if !self.tone_plays {
            return;
        }
        shared.end();
        if let Err(e) = self.wait("the tone fades out", SLOT_WAIT, |_| shared.done()) {
            say(format!("tone: {e}"));
        }
        // Only now: a device event in the tone's tail still counts against it.
        self.pump();
        self.tone_plays = false;
        // A device event since the tone began, or another device now than the one it began on.
        let now = self.host.status();
        if self.tone_device.is_some() && now.as_ref().map(|s| (s.backend, s.block, s.sample_rate)) != self.tone_device {
            let text = now.map_or("no device runs".to_string(), |s| describe(&s));
            self.tone_interrupted.get_or_insert(format!("the device changed under the tone: {text}"));
        }
        if let Some(facts) = &mut self.tone_facts {
            facts.interrupted = self.tone_interrupted.take();
        }
    }

    fn soak(&mut self, seconds: f64) {
        let began = Instant::now();
        let end = began + Duration::from_secs_f64(seconds);
        let (start, mut note) = (self.host.diag(), began + Duration::from_secs(60));
        // Each minute's own block time and diagnostics, so a spike in a long soak has a time.
        let (mut minute, mut load) = (start, self.host.block_load());
        let mut toned = self.tone.as_ref().map(|t| t.counts());
        while Instant::now() < end {
            self.pump();
            if Instant::now() >= note {
                let (diag, now) = (self.host.diag(), self.host.block_load());
                let tone = match (&self.tone, &mut toned) {
                    (Some(shared), Some(before)) => {
                        let counts = shared.counts();
                        let text = format!("; tone this minute: events {}, coverage gaps {}", counts.0 - before.0, counts.1 - before.1);
                        *before = counts;
                        text
                    }
                    _ => String::new(),
                };
                say(format!(
                    "soak {:.0} s: callbacks {}, counters {}; this minute: block {}, asio_phase_slips {}, asio_late_finishes {}, clipped_blocks {}; asio_late_max so far {}{tone}",
                    began.elapsed().as_secs_f64(),
                    diag.callbacks - start.callbacks,
                    moved(&diag, &start),
                    now.since(&load).text(),
                    diag.asio_phase_slips - minute.asio_phase_slips,
                    diag.asio_late_finishes - minute.asio_late_finishes,
                    diag.clipped_blocks - minute.clipped_blocks,
                    diag.asio_late_max
                ));
                (minute, load) = (diag, now);
                note += Duration::from_secs(60);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn switch(&mut self, next: &DeviceRequest) -> Result<(), String> {
        self.finish_grace();
        let mark = self.mark();
        // A `--cycle` round may name the device that runs, and the owner would keep its streams: close
        // them first. `--pause` closes before every switch to WASAPI.
        let running = self.host.status().map(|s| s.backend);
        let close = !next.backend.is_asio() && (self.pause.is_some() || (self.cycled && running == Some(next.backend)));
        if close {
            self.host.close()?;
            std::thread::sleep(self.pause.unwrap_or_default());
        }
        // Only a WASAPI open that starts streams gets a grace window: the owner keeps the streams of the
        // device that runs.
        let opens = !next.backend.is_asio() && (close || running != Some(next.backend));
        let (began, before) = (Instant::now(), self.host.diag());
        // Forced: a switch to another rate drops the loop, which this phase then records again.
        match self.host.open(next.clone(), true) {
            Ok(status) => {
                let rebuilt = status.sample_rate != self.rate;
                if rebuilt {
                    // Another rate builds a new engine, whose counters start from 0: the run's total
                    // holds its xruns alone, so the old engine's forgiven ones leave too.
                    self.forgiven.engine.xruns = 0;
                }
                // An open that outlasts the window gets none: what moved in it stays counted.
                if opens && began.elapsed() < GRACE {
                    let before = if rebuilt { IoDiag { engine: Default::default(), ..before } } else { before };
                    self.grace = Some((before, began + GRACE));
                }
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
                    self.lanes = [None; TRACK_COUNT];
                    self.silence()?;
                    self.record_loop()?;
                    if self.heavy {
                        self.heavy_scene()?;
                    }
                }
            }
            Err(e) => self.fail("switch", format!("to {}: {e}", label(next))),
        }
        self.expect_loop(&format!("the switch to {}", label(next)), self.hold);
        self.phase(&format!("switch to {}", label(next)), &mark);
        if !next.backend.is_asio() {
            join_trace("switch");
        }
        Ok(())
    }

    /// Put the next plugin of the list into slot `k` while the loop plays.
    fn swap(&mut self, k: usize) {
        let mark = self.mark();
        let next = (self.slots[k].plugin + 1) % self.plugins.len();
        self.unload_slot(k);
        self.load_slot(k, next);
        let slot = self.slots[k].slot;
        self.expect_loop(&format!("the swap in slot {slot}"), HOLD);
        self.phase(&format!("swap slot {slot}"), &mark);
    }

    fn drive(&mut self, a: &Args, devices: &Devices, start: &DeviceRequest) -> Result<(), String> {
        let mark = self.mark();
        let status = self.host.open(start.clone(), false)?;
        if !start.backend.is_asio() && mark.at.elapsed() < GRACE {
            self.grace = Some((mark.diag, mark.at + GRACE));
        }
        self.silence()?;
        self.rate = status.sample_rate;
        say(format!("open {}: {}", label(start), describe(&status)));
        if let Some(tone) = a.tone.filter(|&tone| status.input_channels[1] != tone) {
            return Err(format!("--tone {tone}: the device has no such input (slot 1 reads input {})", status.input_channels[1]));
        }
        // `--tone-from`: the tone watches the stream's first seconds, through the setup below.
        if self.tone_from.is_some() {
            self.tone_start()?;
        }
        if let Some(endpoint) = &a.share {
            self.host.set_share(Some(endpoint.clone()))?;
            say(format!("share: the master is mirrored to {endpoint}"));
        }
        for k in 0..self.slots.len() {
            let plugin = self.slots[k].plugin;
            self.load_slot(k, plugin);
        }
        self.phase("open", &mark);

        let mark = self.mark();
        self.record_loop()?;
        self.phase("record", &mark);
        if a.heavy {
            let mark = self.mark();
            self.heavy_scene()?;
            self.phase("heavy scene", &mark);
        }

        self.tone_start()?;
        let mark = self.mark();
        self.soak(a.seconds);
        self.tone_stop();
        let load = self.phase("soak", &mark);
        match (load.quantile(0.999), load.max()) {
            (Some(p999), Some(max)) if (self.heavy || p999 < P999_BAR) && max < MAX_BAR => {}
            (Some(_), Some(_)) => self.fail("load", format!("soak block {}", load.text())),
            _ => self.fail("load", "no callback in the soak".to_string()),
        }
        self.expect_loop("the soak", HOLD);
        if self.heavy && !self.all_play(self.length) {
            self.fail("loop", format!("after the soak: not every lane plays its {} frames: {:?}", self.length, self.lanes));
        }
        if !start.backend.is_asio() {
            join_trace("soak");
        }

        let cycle: Vec<DeviceRequest> = if a.cycle.is_empty() {
            devices.cycle(start, asio_available())
        } else {
            a.cycle.iter().map(|&(backend, buffer)| devices.request(backend, buffer)).collect()
        };
        if a.switches > 0 && cycle.len() == 1 {
            say("no other device to switch to (no ASIO driver cached): every switch opens the one that runs, which keeps its streams (`--cycle wasapi` reopens it)");
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

/// The latest WASAPI run's pushes and pulls from its open (`callback.rs`'s trace), one line each, under
/// a line that says which phase they end.
fn join_trace(after: &str) {
    let lines = super::callback::trace::join_lines();
    say(format!("join trace after the {after}: {} records", lines.len()));
    for line in lines {
        say(format!("j {line}"));
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
    let opened = host.open(request.clone(), false);
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
    let devices = Devices::new(a.input, a.tone, a.device.as_deref())?;
    let start = devices.request(a.backend, a.buffer);
    #[cfg(feature = "asio")]
    if start.backend.is_asio() || (a.switches > 0 && (a.cycle.is_empty() || a.cycle.iter().any(|(backend, _)| backend.is_asio()))) {
        // A standalone DEV process: probe explicitly, with a throwaway sentinel (no app data dir here).
        let sentinel = std::env::temp_dir().join("bleeploop-engine-probe-asio");
        let report = crate::audio_output::probe_asio_startup(&sentinel, true);
        say(format!("asio startup: {}", serde_json::to_string(&report).unwrap_or_default()));
    }
    if a.lag {
        return lag_run(&a, &devices, start);
    }

    let host = EngineHost::new(HostConfig::default());
    // `--tone`: the rig goes into the callback's state while no device runs, as the lag rig does.
    let tone = a.tone.map(|_| Shared::new());
    if let Some(shared) = &tone {
        let _ = host.core.tone.set(shared.clone());
        host.core.rt.lock().map_err(|_| "engine lock poisoned")?.tone = Some(Box::new(ToneRig::new(shared.clone(), a.out == 1, a.tone_level)));
    }
    let mut p = Probe {
        host: host.clone(),
        plugins,
        slots,
        events: Vec::new(),
        lanes: [None; TRACK_COUNT],
        length: 0,
        beats: 0,
        rate: 0,
        mute: a.mute,
        heavy: a.heavy,
        hold: a.hold,
        pause: a.pause,
        cycled: !a.cycle.is_empty(),
        grace: None,
        forgiven: IoDiag::default(),
        tone,
        tone_from: a.tone_from,
        tone_plays: false,
        tone_facts: None,
        tone_device: None,
        tone_interrupted: None,
        fails: Vec::new(),
    };
    if let Err(e) = p.drive(&a, &devices, &start) {
        // A run that stops early says what the join saw, when WASAPI was what ran.
        if host.status().is_some_and(|s| !s.backend.is_asio()) {
            join_trace("failed run");
        }
        p.fail("run", e);
    }
    if a.heavy {
        // The chord's release; a device that is gone drops it.
        let _ = p.send(Command::AllNotesOff);
    }
    // A run that stopped early still fades its tone out.
    p.tone_stop();
    p.finish_grace();
    // The plugins leave the engine while the device still plays (crossfaded out), then it closes.
    for k in 0..p.slots.len() {
        p.unload_slot(k);
    }
    if let Err(e) = host.close() {
        p.fail("run", format!("close: {e}"));
    }
    let tone_rig = p.tone.as_ref().and_then(|_| host.core.rt.lock().ok()?.tone.take());
    p.pump();
    host.shutdown();

    let total = host.diag();
    for line in super::callback::trace::lines() {
        say(format!("trace {line}"));
    }
    say(format!("total: callbacks {}, counters {}", total.callbacks, moved(&total, &IoDiag::default())));
    if let Some(text) = p.forgiven.moved_since(&IoDiag::default()) {
        say(format!("forgiven in WASAPI opens' first {} s: {text}", GRACE.as_secs()));
    }
    let counted = unforgiven(&total, &p.forgiven);
    if counted.faults().iter().any(|(_, n)| *n > 0) {
        p.fails.push(("counters", moved(&counted, &IoDiag::default())));
    }
    let errors = LOG.errors.load(Relaxed);
    if errors > 0 {
        p.fails.push(("log", format!("{errors} error(s) logged")));
    }
    // `--tone`: its closing block, and its check's outcome in words.
    let tone_check = p.tone.as_ref().map(|_| match &tone_rig {
        Some(rig) => {
            let report = rig.report(p.tone_facts.clone());
            for line in &report.lines {
                say(line);
            }
            (report.pass, report.verdict)
        }
        None => (false, "tone: not measured: the rig is gone".to_string()),
    });
    let mut failed = 0;
    for (check, bar) in CHECKS {
        if check == "tone" {
            if let Some((pass, verdict)) = &tone_check {
                failed += usize::from(!pass);
                say(format!("{} {verdict} | {bar}", if *pass { "PASS" } else { "FAIL" }));
            }
            continue;
        }
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

    #[test]
    fn a_grace_window_forgives_only_the_joins_trims_and_starves_and_their_xruns() {
        let before = IoDiag { join_trims: 1, gaps: 2, ..Default::default() };
        let engine = lf_engine::Diag { xruns: 4, commands_dropped: 1, ..Default::default() };
        let now = IoDiag { join_trims: 3, join_starves: 1, gaps: 3, xruns: 1, join_overruns: 1, engine, ..before };
        let f = forgivable(&now, &before);
        assert_eq!(f.moved_since(&IoDiag::default()).as_deref(), Some("join_starves=1 join_trims=2 engine.xruns=4"));
        // The trim before the window, the gaps, the stream's xrun, the overrun and the dropped command stay.
        assert_eq!(
            unforgiven(&now, &f).moved_since(&IoDiag::default()).as_deref(),
            Some("gaps=3 xruns=1 join_overruns=1 join_trims=1 engine.commands_dropped=1")
        );
        // A window that moved nothing forgives nothing.
        assert_eq!(forgivable(&now, &now), IoDiag::default());
    }

    fn args(line: &str) -> Result<Args, String> {
        parse_args(&line.split(' ').map(String::from).collect::<Vec<_>>())
    }

    #[test]
    fn the_tone_needs_asio_the_mute_and_an_input_of_its_own() {
        let a = args("asio 64 --mute --tone 1").ok().expect("the rig's run parses");
        assert_eq!((a.tone, a.input, a.out), (Some(1), 0, 1));
        assert_eq!(args("asio 64 --mute --in 1 --tone 0 --out 0").ok().map(|a| (a.tone, a.out)), Some((Some(0), 0)));
        // Slot 0 keeps `--in`, slot 1 reads the cable.
        let request = Devices::new(a.input, a.tone, None).ok().unwrap().request(a.backend, a.buffer);
        assert_eq!(request.input_channels, [Some(0), Some(1)]);
        assert_eq!(Devices::new(0, None, None).ok().unwrap().request(a.backend, a.buffer).input_channels, [Some(0), Some(0)]);
        for (line, want) in [
            ("asio 64 --tone 1", "--mute"),
            ("asio 64 --mute --tone 0", "same input"),
            ("asio 64 --mute --in 1 --tone 1", "same input"),
            ("asio 64 --mute --tone 1 --lag", "--lag"),
            ("asio 64 --mute --tone 1 --out 2", "--out 0 or 1"),
            ("wasapi default --mute --tone 1", "asio"),
            ("asio 64 --mute --tone", "needs a value"),
        ] {
            let error = args(line).err().unwrap_or_else(|| panic!("`{line}` parsed"));
            assert!(error.contains(want), "`{line}`: {error}");
        }
    }

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
