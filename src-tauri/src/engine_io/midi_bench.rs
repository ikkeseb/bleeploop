//! OWNS (DEV builds only): the MIDI latency benchmark and the `settings`/`ends` lock waits, the
//! measurement of `docs/plans/native-midi.md` § Measurement. How to run it and what each number means:
//! `docs/VERIFY.md` § MIDI latency benchmark. Nothing here runs unless an environment variable read at
//! startup asks (`LF_MIDI_BENCH`, `LF_LOCK_WAITS`: [`start_from_env`]); a release build compiles none
//! of it (`mod.rs` declares the module under `debug_assertions`).
//!
//! - **The sender:** a midir output on this module's own thread sends note-ons to a loopback port at a
//!   fixed interval, independent of the UI, each with its note-off half an interval later, and stamps
//!   each send's `Instant`. The app receives them as it receives any controller (Web MIDI today, native
//!   input after the switch), so one sender and one clock serve before and after.
//! - **The sequence id is the (note, velocity) pair:** send `i` plays note `base + i % notes` at
//!   velocity `1 + (i / notes) % 127` ([`key_of`]), a pair that comes back only `notes × 127` sends
//!   later. A record of that pair matches the oldest send of it still waiting; a send not matched
//!   within the timeout is lost, and a record with no send waiting is a duplicate ([`match_seen`]).
//!   Notes outside the benchmark's pairs (the player's own) are strays, left out.
//! - **Applied:** the engine records each `NoteOn` it applies with its frame
//!   (`lf_engine::note_record`), and FrameClock's stamp history turns the frame into an instant
//!   (`frame_clock::frame_instant`). **Arrival** (a diagnostic): [`arrived`], stamped where a note
//!   reaches native code: `engine_send` for Web MIDI's notes (after the WebView's handler and the IPC),
//!   the native input's handler after the switch.
//! - **UI stalls:** the DEV frontend runs a long main-thread task every few seconds when the benchmark
//!   asks (`src/platform/host.tauri.ts`, [`midi_bench_stall_plan`]) and reports each one's window
//!   ([`midi_bench_stall`]); a note sent inside a window counts as inside a stall.
//! - **Lock waits:** how long `settings` and `ends` takers waited to get them (`mod.rs` `lock_at`), per
//!   lock and taker: the input path (`EngineHost::send_all`), the feed (`drain_feed`), the rest. Counted
//!   in every DEV run; `LF_LOCK_WAITS=<seconds>` logs the totals that often.
//!
//! The reader of the engine's applied-note record is taken under the engine lock (`Rt`), with a
//! `try_lock` held for one `Option::take`: a callback that comes in that moment misses the lock once
//! (counted in `lock_misses`). It happens once per engine, in the warm-up.

use std::collections::{HashMap, VecDeque};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::{Acquire, Relaxed, Release}};
use std::sync::{Mutex, OnceLock, TryLockError};
use std::time::{Duration, Instant};

use lf_engine::grid::Frame;
use lf_engine::note_record::{AppliedNote, AppliedNotes};
use lf_engine::Command;
use serde::Serialize;

use super::frame_clock::{block_of, frame_instant, stamp, Stamp};
use super::{EngineHost, LockSite};

// ── Configuration ─────────────────────────────────────────────────────────────────────────────────

/// The stalls the DEV frontend runs: a busy main thread for `ms`, every `every_ms`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StallPlan {
    pub every_ms: u64,
    pub ms: u64,
}

/// `LF_MIDI_BENCH`, parsed: `"<port name part>;interval_ms=25;notes=24;…"` (keys: [`Config::parse`]).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Config {
    /// Part of the loopback output port's name (case-insensitive); exactly one port must match.
    port: String,
    interval: Duration,
    /// Note-ons sent back to back at each interval (distinct notes).
    burst: u32,
    /// The notes the pairs cycle through: `base`, `base + 1`, … `base + notes - 1`.
    base: u8,
    notes: u8,
    /// Note-ons measured (after the warm-up).
    count: u64,
    warmup: Duration,
    /// A send not applied within this is lost.
    timeout: Duration,
    stalls: Option<StallPlan>,
    /// Names the run's files (`before`, `after`).
    label: String,
    /// The folder the report goes to; default the repo's `logs/`.
    out: Option<PathBuf>,
}

impl Config {
    /// `;`-separated: a part without `=` is the port, then `port=`, `interval_ms=` (25), `burst=` (1),
    /// `base=` (48), `notes=` (24), `count=` (2000), `warmup_s=` (5), `timeout_ms=` (1000),
    /// `stall_every_ms=` (none: no stalls), `stall_ms=` (250), `label=` (`run`), `out=`.
    pub(crate) fn parse(spec: &str) -> Result<Config, String> {
        let mut c = Config {
            port: String::new(),
            interval: Duration::from_millis(25),
            burst: 1,
            base: 48,
            notes: 24,
            count: 2000,
            warmup: Duration::from_secs(5),
            timeout: Duration::from_millis(1000),
            stalls: None,
            label: "run".into(),
            out: None,
        };
        let (mut stall_every, mut stall_ms) = (None, 250);
        for part in spec.split(';').map(str::trim).filter(|p| !p.is_empty()) {
            let Some((key, value)) = part.split_once('=') else {
                c.port = part.to_string();
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            let number = |what: &str| value.parse::<u64>().map_err(|_| format!("{what}={value}: not a whole number"));
            match key {
                "port" => c.port = value.to_string(),
                "interval_ms" => c.interval = Duration::from_millis(number(key)?),
                "burst" => c.burst = number(key)? as u32,
                "base" => c.base = u8::try_from(number(key)?).map_err(|_| format!("base={value}: over 127"))?,
                "notes" => c.notes = u8::try_from(number(key)?).map_err(|_| format!("notes={value}: over 127"))?,
                "count" => c.count = number(key)?,
                "warmup_s" => c.warmup = Duration::from_secs(number(key)?),
                "timeout_ms" => c.timeout = Duration::from_millis(number(key)?),
                "stall_every_ms" => stall_every = Some(number(key)?),
                "stall_ms" => stall_ms = number(key)?,
                "label" => c.label = value.chars().filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_').collect(),
                "out" => c.out = Some(PathBuf::from(value)),
                _ => return Err(format!("unknown key {key}")),
            }
        }
        if c.port.is_empty() {
            return Err("no port named (the first part, or port=)".into());
        }
        if c.interval < Duration::from_millis(2) {
            return Err("interval_ms below 2".into());
        }
        if c.notes == 0 || c.base as u16 + c.notes as u16 > 128 {
            return Err(format!("notes {}..{} leave 0..127", c.base, c.base as u16 + c.notes as u16));
        }
        if c.burst == 0 || c.burst > c.notes as u32 {
            return Err(format!("burst={} must be 1..={} (each note of a burst distinct)", c.burst, c.notes));
        }
        if c.count == 0 {
            return Err("count=0".into());
        }
        if let Some(every_ms) = stall_every {
            if stall_ms == 0 || stall_ms >= every_ms {
                return Err(format!("stall_ms={stall_ms} must be 1..stall_every_ms={every_ms}"));
            }
            c.stalls = Some(StallPlan { every_ms, ms: stall_ms });
        }
        if c.label.is_empty() {
            c.label = "run".into();
        }
        Ok(c)
    }

    fn space(&self) -> Space {
        Space { base: self.base, notes: self.notes }
    }
}

// ── Sequence ids, matching and the numbers (pure) ────────────────────────────────────────────────

/// A note and its MIDI velocity: a send's sequence id while it is in flight.
pub(crate) type Key = (u8, u8);

/// The pairs the benchmark sends.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Space {
    pub base: u8,
    pub notes: u8,
}

/// Send `seq`'s pair. Velocity is never 0 (a note-off).
pub(crate) fn key_of(seq: u64, space: Space) -> Key {
    let notes = space.notes as u64;
    (space.base + (seq % notes) as u8, 1 + ((seq / notes) % 127) as u8)
}

/// The pair a `NoteOn` carries (velocity 0..1 as the engine gets it, `v / 127`), when it is one of the
/// benchmark's.
pub(crate) fn bench_key(note: u8, velocity: f32, space: Space) -> Option<Key> {
    let v = (velocity * 127.0).round();
    let ours = note >= space.base && (note as u16) < space.base as u16 + space.notes as u16 && (1.0..=127.0).contains(&v);
    ours.then_some((note, v as u8))
}

/// One send, or one record of a note (applied or arrived): its pair and instant (ns, `frame_clock::stamp`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Seen {
    pub key: Key,
    pub at: u64,
}

/// Which record each send matched (an index into `seen`), and the records no send was waiting for.
#[derive(Debug, PartialEq)]
pub(crate) struct Matching {
    pub by_send: Vec<Option<usize>>,
    pub duplicates: usize,
}

/// How much earlier than its send a record may be and still match it: the applied instant is the
/// rendering block's entry, which can precede the engine's take of a command pushed just after it.
const EARLY: u64 = 5_000_000;

/// Match `seen` (any order) to `sent` (in send order): each record takes the oldest send of its pair
/// still waiting; a send older than `timeout` when a record of its pair comes is lost, and a record no
/// send of its pair waits for is a duplicate.
pub(crate) fn match_seen(sent: &[Seen], seen: &[Seen], timeout: u64) -> Matching {
    let mut waiting: HashMap<Key, VecDeque<usize>> = HashMap::new();
    for (i, s) in sent.iter().enumerate() {
        waiting.entry(s.key).or_default().push_back(i);
    }
    let mut order: Vec<usize> = (0..seen.len()).collect();
    order.sort_by_key(|&j| seen[j].at);
    let mut by_send = vec![None; sent.len()];
    let mut duplicates = 0;
    for j in order {
        let r = seen[j];
        let Some(queue) = waiting.get_mut(&r.key) else {
            duplicates += 1;
            continue;
        };
        while queue.front().is_some_and(|&i| sent[i].at.saturating_add(timeout) < r.at) {
            queue.pop_front();
        }
        match queue.front() {
            Some(&i) if sent[i].at <= r.at + EARLY => {
                by_send[i] = Some(j);
                queue.pop_front();
            }
            _ => duplicates += 1,
        }
    }
    Matching { by_send, duplicates }
}

/// p50, p99 and max of a set of durations, in ms; nearest rank (the smallest value with at least that
/// share of the set at or under it).
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub(crate) struct Spread {
    pub n: usize,
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

pub(crate) fn spread(mut ns: Vec<i64>) -> Option<Spread> {
    if ns.is_empty() {
        return None;
    }
    ns.sort_unstable();
    let rank = |q: f64| ns[((q * ns.len() as f64).ceil() as usize).clamp(1, ns.len()) - 1];
    let ms = |v: i64| (v as f64 / 1e3).round() / 1e3;
    Some(Spread { n: ns.len(), p50_ms: ms(rank(0.5)), p99_ms: ms(rank(0.99)), max_ms: ms(ns[ns.len() - 1]) })
}

/// Whether `at` falls inside one of `windows` (start inclusive, end exclusive; any order).
pub(crate) fn in_stall(windows: &[(u64, u64)], at: u64) -> bool {
    windows.iter().any(|&(start, end)| start <= at && at < end)
}

/// Callbacks that entered after a note arrived and still did not apply it: 0 when the first block after
/// its arrival did. `stamps` as `frame_instant`'s.
pub(crate) fn blocks_late(stamps: &[Stamp], arrived: u64, applied: Frame) -> Option<usize> {
    let k = block_of(stamps, applied)?;
    Some(k.saturating_sub(stamps.partition_point(|s| s.entry_ns <= arrived)))
}

/// Breaks in the device clock: a block that does not start where the one before ended (a WASAPI dry
/// jump), or the device stopping (`clear`'s marker).
pub(crate) fn discontinuities(stamps: &[Stamp]) -> u64 {
    stamps.windows(2).filter(|w| w[0].rate != 0 && (w[1].rate == 0 || w[1].frame != w[0].frame + w[0].block as Frame)).count() as u64
}

/// What a run collected.
#[derive(Default)]
pub(crate) struct Collected {
    pub sent: Vec<Seen>,
    pub applied: Vec<AppliedNote>,
    /// (note, velocity 0..1, instant)
    pub arrivals: Vec<(u8, f32, u64)>,
    /// Every stamp, `clear` markers included.
    pub stamps: Vec<Stamp>,
    pub stalls: Vec<(u64, u64)>,
}

/// The timings split by where the send fell.
#[derive(Default, Debug, PartialEq)]
pub(crate) struct Side {
    pub sent: usize,
    pub sender_to_applied: Vec<i64>,
    pub arrival_to_applied: Vec<i64>,
}

#[derive(Debug, PartialEq)]
pub(crate) struct Outcome {
    pub applied: usize,
    pub lost: usize,
    pub duplicates: usize,
    pub strays: usize,
    /// Applied notes whose frame no stamp's block holds.
    pub unconverted: usize,
    pub arrived: usize,
    pub inside: Side,
    pub outside: Side,
    /// Per note with an arrival: blocks late ([`blocks_late`]).
    pub late: Vec<usize>,
}

pub(crate) fn evaluate(c: &Collected, space: Space, timeout: Duration) -> Outcome {
    let running: Vec<Stamp> = c.stamps.iter().copied().filter(|s| s.rate != 0).collect();
    let (mut strays, mut unconverted) = (0, 0);
    let mut applied: Vec<(Seen, Frame)> = Vec::new();
    for a in &c.applied {
        let Some(key) = bench_key(a.note, a.velocity, space) else {
            strays += 1;
            continue;
        };
        match frame_instant(&running, a.frame) {
            Some(at) => applied.push((Seen { key, at }, a.frame)),
            None => unconverted += 1,
        }
    }
    let arrivals: Vec<Seen> = c.arrivals.iter().filter_map(|&(note, v, at)| bench_key(note, v, space).map(|key| Seen { key, at })).collect();
    let timeout = timeout.as_nanos() as u64;
    let seen: Vec<Seen> = applied.iter().map(|(s, _)| *s).collect();
    let by_applied = match_seen(&c.sent, &seen, timeout);
    let by_arrival = match_seen(&c.sent, &arrivals, timeout);
    let mut o = Outcome {
        applied: 0,
        lost: 0,
        duplicates: by_applied.duplicates,
        strays,
        unconverted,
        arrived: by_arrival.by_send.iter().flatten().count(),
        inside: Side::default(),
        outside: Side::default(),
        late: Vec::new(),
    };
    for (i, send) in c.sent.iter().enumerate() {
        let side = if in_stall(&c.stalls, send.at) { &mut o.inside } else { &mut o.outside };
        side.sent += 1;
        let Some(j) = by_applied.by_send[i] else {
            o.lost += 1;
            continue;
        };
        o.applied += 1;
        let (at, frame) = (applied[j].0.at, applied[j].1);
        side.sender_to_applied.push(at as i64 - send.at as i64);
        if let Some(a) = by_arrival.by_send[i] {
            side.arrival_to_applied.push(at as i64 - arrivals[a].at as i64);
            o.late.extend(blocks_late(&running, arrivals[a].at, frame));
        }
    }
    o
}

// ── Lock waits ────────────────────────────────────────────────────────────────────────────────────

/// The upper bounds (ns) of the wait histogram's bins; the last bin is everything at or over 10 ms.
const WAIT_BOUNDS: [u64; 5] = [1_000, 10_000, 100_000, 1_000_000, 10_000_000];
const WAIT_BIN_NAMES: [&str; 6] = ["<1us", "<10us", "<100us", "<1ms", "<10ms", ">=10ms"];
const SITES: usize = 6;
const SITE_NAMES: [&str; SITES] = ["settings.send", "ends.send", "settings.feed", "ends.feed", "settings.other", "ends.other"];

fn site_index(site: LockSite) -> usize {
    match site {
        LockSite::SettingsSend => 0,
        LockSite::EndsSend => 1,
        LockSite::SettingsFeed => 2,
        LockSite::EndsFeed => 3,
        LockSite::Settings => 4,
        LockSite::Ends => 5,
    }
}

struct WaitStat {
    count: AtomicU64,
    total_ns: AtomicU64,
    max_ns: AtomicU64,
    bins: [AtomicU64; 6],
}

impl WaitStat {
    const fn new() -> WaitStat {
        WaitStat { count: AtomicU64::new(0), total_ns: AtomicU64::new(0), max_ns: AtomicU64::new(0), bins: [const { AtomicU64::new(0) }; 6] }
    }
}

/// Every DEV run's lock waits, process-wide.
pub(crate) struct LockWaits([WaitStat; SITES]);

pub(crate) static LOCK_WAITS: LockWaits = LockWaits([const { WaitStat::new() }; SITES]);

/// A copy of one taker's waits; `max_ns` is since the process started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct WaitSnap {
    pub count: u64,
    pub total_ns: u64,
    pub max_ns: u64,
    pub bins: [u64; 6],
}

impl WaitSnap {
    fn since(&self, before: &WaitSnap) -> WaitSnap {
        WaitSnap {
            count: self.count - before.count,
            total_ns: self.total_ns - before.total_ns,
            max_ns: self.max_ns,
            bins: std::array::from_fn(|k| self.bins[k] - before.bins[k]),
        }
    }
}

impl LockWaits {
    pub(crate) fn record(&self, site: LockSite, waited: Duration) {
        let stat = &self.0[site_index(site)];
        let ns = waited.as_nanos().min(u64::MAX as u128) as u64;
        stat.count.fetch_add(1, Relaxed);
        stat.total_ns.fetch_add(ns, Relaxed);
        stat.max_ns.fetch_max(ns, Relaxed);
        stat.bins[WAIT_BOUNDS.partition_point(|&b| b <= ns)].fetch_add(1, Relaxed);
    }

    fn snapshot(&self) -> [WaitSnap; SITES] {
        std::array::from_fn(|k| {
            let s = &self.0[k];
            WaitSnap { count: s.count.load(Relaxed), total_ns: s.total_ns.load(Relaxed), max_ns: s.max_ns.load(Relaxed), bins: std::array::from_fn(|b| s.bins[b].load(Relaxed)) }
        })
    }
}

/// One line: each taker that took its lock, `name n=… total=…us max=…us <1us=… …`.
fn waits_text(snaps: &[WaitSnap; SITES]) -> String {
    let mut parts = Vec::new();
    for (name, s) in SITE_NAMES.iter().zip(snaps) {
        if s.count == 0 {
            continue;
        }
        let bins: Vec<String> = WAIT_BIN_NAMES.iter().zip(s.bins).filter(|(_, n)| *n > 0).map(|(b, n)| format!("{b}={n}")).collect();
        parts.push(format!("{name} n={} total={}us max={}us {}", s.count, s.total_ns / 1000, s.max_ns / 1000, bins.join(" ")));
    }
    if parts.is_empty() { "none".into() } else { parts.join("; ") }
}

fn waits_json(snaps: &[WaitSnap; SITES]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (name, s) in SITE_NAMES.iter().zip(snaps) {
        let bins: serde_json::Map<String, serde_json::Value> = WAIT_BIN_NAMES.iter().zip(s.bins).map(|(b, n)| (b.to_string(), n.into())).collect();
        map.insert(name.to_string(), serde_json::json!({ "count": s.count, "total_us": s.total_ns / 1000, "max_us_since_start": s.max_ns / 1000, "bins": bins }));
    }
    map.into()
}

// ── What the app records while a benchmark is armed ──────────────────────────────────────────────

/// Arrivals are stamped only while a benchmark measures.
static ARMED: AtomicBool = AtomicBool::new(false);
/// Bounded: a benchmark drains them every few milliseconds.
const MAX_ARRIVALS: usize = 1 << 16;
static ARRIVALS: Mutex<Vec<(u8, f32, u64)>> = Mutex::new(Vec::new());
static ARRIVALS_DROPPED: AtomicU64 = AtomicU64::new(0);
/// The stall windows the frontend reported, (start, end) in ns.
static STALLS: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());
/// The frontend's stall plan, set at startup; the frontend stalls while `STALLING` holds.
static PLAN: OnceLock<StallPlan> = OnceLock::new();
static STALLING: AtomicBool = AtomicBool::new(false);

/// A batch of commands reached native code at `at` (`engine_send`; the native input after the switch):
/// each `NoteOn`'s arrival, while a benchmark measures. One atomic load otherwise.
pub fn arrived<'a>(commands: impl IntoIterator<Item = &'a Command>, at: Instant) {
    if !ARMED.load(Relaxed) {
        return;
    }
    let at = stamp(at);
    let mut arrivals = ARRIVALS.lock().unwrap_or_else(|e| e.into_inner());
    for command in commands {
        if let Command::NoteOn(note, velocity) = *command {
            if arrivals.len() < MAX_ARRIVALS {
                arrivals.push((note, velocity, at));
            } else {
                ARRIVALS_DROPPED.fetch_add(1, Relaxed);
            }
        }
    }
}

/// DEV: the stalls the frontend should run, while a benchmark that asked for them has not ended.
#[tauri::command]
pub fn midi_bench_stall_plan() -> Option<StallPlan> {
    STALLING.load(Acquire).then(|| PLAN.get().copied()).flatten()
}

/// DEV: the frontend's main thread was busy for `ms`, ending `ago_ms` before this call (both from
/// `performance.now()`; the window's edges carry the IPC's delay). False once the benchmark ended:
/// the frontend stops stalling.
#[tauri::command]
pub fn midi_bench_stall(ago_ms: f64, ms: f64) -> bool {
    let now = stamp(Instant::now());
    if ago_ms.is_finite() && ms.is_finite() && ago_ms >= 0.0 && ms >= 0.0 {
        let start = now.saturating_sub(((ago_ms + ms) * 1e6) as u64);
        let mut stalls = STALLS.lock().unwrap_or_else(|e| e.into_inner());
        if stalls.len() < MAX_ARRIVALS {
            stalls.push((start, start + (ms * 1e6) as u64));
        }
    }
    STALLING.load(Acquire)
}

// ── The run ──────────────────────────────────────────────────────────────────────────────────────

/// At startup: `LF_LOCK_WAITS=<seconds>` logs the lock waits that often; `LF_MIDI_BENCH` starts a
/// benchmark on its own thread. Neither set: nothing runs.
pub fn start_from_env(host: &EngineHost) {
    if let Ok(every) = std::env::var("LF_LOCK_WAITS") {
        match every.trim().parse::<u64>() {
            Ok(secs) if secs > 0 => {
                let spawned = std::thread::Builder::new().name("lf-lock-waits".into()).spawn(move || {
                    let began = Instant::now();
                    loop {
                        std::thread::sleep(Duration::from_secs(secs));
                        log::info!("[lock-waits] {} s: {}", began.elapsed().as_secs(), waits_text(&LOCK_WAITS.snapshot()));
                    }
                });
                if let Err(e) = spawned {
                    log::error!("[lock-waits] no thread ({e})");
                }
            }
            _ => log::error!("[lock-waits] LF_LOCK_WAITS={every}: not a whole number of seconds over 0"),
        }
    }
    let Ok(spec) = std::env::var("LF_MIDI_BENCH") else { return };
    let config = match Config::parse(&spec) {
        Ok(config) => config,
        Err(e) => {
            log::error!("[midi-bench] LF_MIDI_BENCH: {e}; no benchmark runs");
            return;
        }
    };
    if let Some(plan) = config.stalls {
        let _ = PLAN.set(plan);
        STALLING.store(true, Release);
    }
    let host = host.clone();
    let spawned = std::thread::Builder::new().name("lf-midi-bench".into()).spawn(move || {
        let result = run(&host, &config);
        ARMED.store(false, Relaxed);
        STALLING.store(false, Release);
        if let Err(e) = result {
            log::error!("[midi-bench] {e}");
        }
    });
    if let Err(e) = spawned {
        log::error!("[midi-bench] no thread ({e})");
    }
}

/// How long the sender waits for its port to appear.
const PORT_WAIT: Duration = Duration::from_secs(60);
/// How often the run drains what was recorded between sends and while it waits.
const DRAIN_EVERY: Duration = Duration::from_millis(10);
/// The quiet after the warm-up, for its last notes to land before the measured ones start.
const SETTLE: Duration = Duration::from_secs(1);

fn run(host: &EngineHost, config: &Config) -> Result<(), String> {
    log::info!("[midi-bench] {config:?}");
    let mut sender = Sender::open(config)?;
    let mut logged = false;
    while host.status().is_none() {
        if !logged {
            log::info!("[midi-bench] waiting for the audio device to run");
            logged = true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let mut tap = Tap::new(host)?;
    ARMED.store(true, Relaxed);
    log::info!("[midi-bench] warm-up: {} s", config.warmup.as_secs());
    let mut warm = Collected::default();
    sender.play(config, None, config.warmup, &mut tap, &mut warm)?;
    tap.wait(SETTLE, &mut warm);
    let heard = warm.applied.iter().filter(|a| bench_key(a.note, a.velocity, config.space()).is_some()).count();
    if heard == 0 {
        log::warn!("[midi-bench] no note reached the engine in the warm-up: is the app listening on the port's input, and an engine running?");
    }
    let before = host.diag();
    let waits_before = LOCK_WAITS.snapshot();
    let refused_before = tap.refused();
    let (rebuilds_before, stamps_lost_before, sends_failed_before) = (tap.rebuilds, tap.stamps_lost, sender.failed);
    let arrivals_dropped_before = ARRIVALS_DROPPED.load(Relaxed);
    let started_unix_s = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let mut run = Collected::default();
    log::info!("[midi-bench] measuring {} notes every {} ms", config.count, config.interval.as_millis());
    sender.play(config, Some(config.count), Duration::MAX, &mut tap, &mut run)?;
    tap.wait(config.timeout + Duration::from_millis(500), &mut run);
    ARMED.store(false, Relaxed);
    STALLING.store(false, Release);
    let after = host.diag();
    let outcome = evaluate(&run, config.space(), config.timeout);
    let counters = Counters {
        callbacks: after.callbacks - before.callbacks,
        xruns: after.xruns - before.xruns,
        gaps: after.gaps - before.gaps,
        lock_misses: after.lock_misses - before.lock_misses,
        engine_xruns: after.engine.xruns.saturating_sub(before.engine.xruns),
        commands_full: after.commands_full - before.commands_full,
        engine_commands_dropped: after.engine.commands_dropped.saturating_sub(before.engine.commands_dropped),
        clock_discontinuities: discontinuities(&run.stamps),
        stamps_lost: tap.stamps_lost - stamps_lost_before,
        engine_rebuilds: tap.rebuilds - rebuilds_before,
        record_refused: tap.refused() - refused_before,
        arrivals_dropped: ARRIVALS_DROPPED.load(Relaxed) - arrivals_dropped_before,
        sends_failed: sender.failed - sends_failed_before,
        moved: after.moved_since(&before),
    };
    let waits_after = LOCK_WAITS.snapshot();
    let waits = std::array::from_fn(|k| waits_after[k].since(&waits_before[k]));
    report(host, config, started_unix_s, &run, &outcome, &counters, &waits)
}

#[derive(Debug, Serialize)]
struct Counters {
    callbacks: u64,
    xruns: u64,
    gaps: u64,
    lock_misses: u64,
    engine_xruns: u64,
    commands_full: u64,
    engine_commands_dropped: u64,
    clock_discontinuities: u64,
    stamps_lost: u64,
    engine_rebuilds: u64,
    record_refused: u64,
    arrivals_dropped: u64,
    sends_failed: u64,
    moved: Option<String>,
}

/// The loopback output and the run's sequence.
struct Sender {
    out: midir::MidiOutputConnection,
    /// Sends midir refused.
    failed: u64,
}

impl Sender {
    /// Connect to the one output port whose name holds `config.port`, waiting up to `PORT_WAIT` for it.
    fn open(config: &Config) -> Result<Sender, String> {
        let deadline = Instant::now() + PORT_WAIT;
        let wanted = config.port.to_lowercase();
        let mut logged = false;
        loop {
            let output = midir::MidiOutput::new("BleepLoop bench").map_err(|e| format!("midir: {e}"))?;
            let ports = output.ports();
            let names: Vec<String> = ports.iter().map(|p| output.port_name(p).unwrap_or_default()).collect();
            let hits: Vec<usize> = (0..names.len()).filter(|&k| names[k].to_lowercase().contains(&wanted)).collect();
            match hits[..] {
                [k] => {
                    log::info!("[midi-bench] sending to \"{}\"", names[k]);
                    let out = output.connect(&ports[k], "BleepLoop bench").map_err(|e| format!("connecting to \"{}\": {e}", names[k]))?;
                    return Ok(Sender { out, failed: 0 });
                }
                [] if Instant::now() < deadline => {
                    if !logged {
                        log::info!("[midi-bench] waiting for an output port named like \"{}\" (present: {names:?})", config.port);
                        logged = true;
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
                [] => return Err(format!("no MIDI output port named like \"{}\" (present: {names:?})", config.port)),
                _ => return Err(format!("several MIDI output ports named like \"{}\": {:?}", config.port, hits.iter().map(|&k| &names[k]).collect::<Vec<_>>())),
            }
        }
    }

    /// Send `count` note-ons (or for `length`), `config.burst` at a time every interval, each burst's
    /// note-offs half an interval later; stamp each note-on into `into.sent` and drain the tap between.
    fn play(&mut self, config: &Config, count: Option<u64>, length: Duration, tap: &mut Tap, into: &mut Collected) -> Result<(), String> {
        let space = config.space();
        let start = Instant::now() + DRAIN_EVERY;
        let mut seq = 0u64;
        for tick in 0u32.. {
            let at = start + config.interval * tick;
            if count.is_some_and(|n| seq >= n) || at.saturating_duration_since(start) >= length {
                return Ok(());
            }
            tap.wait_until(at, into);
            let mut keys = Vec::with_capacity(config.burst as usize);
            for _ in 0..config.burst {
                if count.is_some_and(|n| seq >= n) {
                    break;
                }
                let key = key_of(seq, space);
                seq += 1;
                let sent = stamp(Instant::now());
                if self.out.send(&[0x90, key.0, key.1]).is_ok() {
                    into.sent.push(Seen { key, at: sent });
                    keys.push(key);
                } else {
                    self.failed += 1;
                }
            }
            tap.drain(into);
            tap.wait_until(at + config.interval / 2, into);
            for key in keys {
                if self.out.send(&[0x80, key.0, 0]).is_err() {
                    self.failed += 1;
                }
            }
            tap.drain(into);
        }
        Ok(())
    }
}

/// The run's reader of everything recorded: the engine's applied notes, the clock's stamps, the
/// arrivals and the stalls.
struct Tap<'a> {
    host: &'a EngineHost,
    reader: Option<AppliedNotes>,
    /// The engine generation the reader belongs to.
    generation: u64,
    next_stamp: u64,
    stamps_lost: u64,
    rebuilds: u64,
    /// What earlier engines' readers refused.
    refused_before: u64,
}

impl<'a> Tap<'a> {
    fn new(host: &'a EngineHost) -> Result<Tap<'a>, String> {
        let generation = host.core.engine_gen.load(Acquire);
        let reader = take_reader(host).ok_or("the engine's applied-note record was not reachable (taken already, or no engine)")?;
        let mut tap = Tap { host, reader: Some(reader), generation, next_stamp: host.core.clock.stamps_written(), stamps_lost: 0, rebuilds: 0, refused_before: 0 };
        tap.drain(&mut Collected::default());
        STALLS.lock().unwrap_or_else(|e| e.into_inner()).clear();
        ARRIVALS.lock().unwrap_or_else(|e| e.into_inner()).clear();
        Ok(tap)
    }

    fn refused(&self) -> u64 {
        self.refused_before + self.reader.as_ref().map_or(0, AppliedNotes::refused)
    }

    /// Move what was recorded since the last call into `into`; after an engine rebuild, finish the old
    /// engine's record and take the new one's.
    fn drain(&mut self, into: &mut Collected) {
        let generation = self.host.core.engine_gen.load(Acquire);
        if let Some(reader) = &mut self.reader {
            while let Ok(note) = reader.rx.pop() {
                into.applied.push(note);
            }
        }
        if generation != self.generation {
            self.refused_before += self.reader.take().map_or(0, |r| r.refused());
            self.reader = take_reader(self.host);
            if self.reader.is_some() {
                self.generation = generation;
                self.rebuilds += 1;
            }
        }
        self.stamps_lost += self.host.core.clock.stamps_since(&mut self.next_stamp, &mut into.stamps);
        into.arrivals.append(&mut ARRIVALS.lock().unwrap_or_else(|e| e.into_inner()));
        into.stalls.append(&mut STALLS.lock().unwrap_or_else(|e| e.into_inner()));
    }

    fn wait_until(&mut self, at: Instant, into: &mut Collected) {
        loop {
            let now = Instant::now();
            if now >= at {
                return;
            }
            let left = at - now;
            if left > DRAIN_EVERY * 2 {
                std::thread::sleep(DRAIN_EVERY);
                self.drain(into);
            } else {
                std::thread::sleep(left);
            }
        }
    }

    fn wait(&mut self, length: Duration, into: &mut Collected) {
        self.wait_until(Instant::now() + length, into);
        self.drain(into);
    }
}

/// Take the reader of the running engine's applied-note record: `try_lock` until the callback is out,
/// held for the take alone (`Option::take`). `None` without an engine, or when its reader is gone.
fn take_reader(host: &EngineHost) -> Option<AppliedNotes> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match host.core.rt.try_lock() {
            Ok(mut rt) => return rt.engine.as_mut().and_then(lf_engine::Engine::take_applied_notes),
            Err(TryLockError::Poisoned(e)) => return e.into_inner().engine.as_mut().and_then(lf_engine::Engine::take_applied_notes),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => std::thread::sleep(Duration::from_micros(200)),
            Err(TryLockError::WouldBlock) => return None,
        }
    }
}

/// Write `logs/midi-bench-<label>-<unix s>.json`, append the summary line to `logs/midi-bench.log`,
/// and log it.
fn report(host: &EngineHost, config: &Config, started_unix_s: u64, run: &Collected, o: &Outcome, counters: &Counters, waits: &[WaitSnap; SITES]) -> Result<(), String> {
    let s2a = |side: &Side| spread(side.sender_to_applied.clone());
    let a2a = |side: &Side| spread(side.arrival_to_applied.clone());
    let sender_to_applied = spread([o.inside.sender_to_applied.as_slice(), &o.outside.sender_to_applied].concat());
    let arrival_to_applied = spread([o.inside.arrival_to_applied.as_slice(), &o.outside.arrival_to_applied].concat());
    let late_notes = o.late.iter().filter(|&&k| k > 0).count();
    let stall_ms: u64 = run.stalls.iter().map(|(a, b)| (b - a) / 1_000_000).sum();
    let side = |side: &Side| serde_json::json!({ "sent": side.sent, "sender_to_applied": s2a(side), "arrival_to_applied": a2a(side) });
    let json = serde_json::json!({
        "label": config.label,
        "port": config.port,
        "started_unix_s": started_unix_s,
        "config": {
            "interval_ms": config.interval.as_millis() as u64,
            "burst": config.burst,
            "base": config.base,
            "notes": config.notes,
            "count": config.count,
            "warmup_s": config.warmup.as_secs(),
            "timeout_ms": config.timeout.as_millis() as u64,
            "stalls": config.stalls,
        },
        "device": host.status(),
        "sent": run.sent.len(),
        "applied": o.applied,
        "lost": o.lost,
        "duplicates": o.duplicates,
        "strays": o.strays,
        "unconverted": o.unconverted,
        "arrived": o.arrived,
        "sender_to_applied": sender_to_applied,
        "arrival_to_applied_diagnostic": arrival_to_applied,
        "inside_stalls": side(&o.inside),
        "outside_stalls": side(&o.outside),
        "stalls": { "count": run.stalls.len(), "total_ms": stall_ms },
        "queue": { "judged": o.late.len(), "late_notes": late_notes, "max_blocks_late": o.late.iter().max() },
        "counters": counters,
        "lock_waits": waits_json(waits),
    });
    let fmt = |s: Option<Spread>| s.map_or("none".into(), |s| format!("p50={} p99={} max={} ms", s.p50_ms, s.p99_ms, s.max_ms));
    let summary = format!(
        "[midi-bench] {} sent={} applied={} lost={} dup={} stray={} | sender->applied {} | arrival->applied (diag) {} | in stalls n={} {} | outside {} | late={} max_blocks={} | callbacks={} xruns={} engine_xruns={} gaps={} lock_misses={} discontinuities={}",
        config.label,
        run.sent.len(),
        o.applied,
        o.lost,
        o.duplicates,
        o.strays,
        fmt(sender_to_applied),
        fmt(arrival_to_applied),
        o.inside.sent,
        fmt(s2a(&o.inside)),
        fmt(s2a(&o.outside)),
        late_notes,
        o.late.iter().max().map_or("none".into(), |k| k.to_string()),
        counters.callbacks,
        counters.xruns,
        counters.engine_xruns,
        counters.gaps,
        counters.lock_misses,
        counters.clock_discontinuities,
    );
    log::info!("{summary}");
    log::info!("[midi-bench] lock waits during the run: {}", waits_text(waits));
    let dir = config.out.clone().unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("logs"));
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(format!("midi-bench-{}-{started_unix_s}.json", config.label));
    let text = serde_json::to_string_pretty(&json).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    let line = dir.join("midi-bench.log");
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&line)
        .and_then(|mut f| writeln!(f, "{summary} | {}", path.display()))
        .map_err(|e| format!("{}: {e}", line.display()))?;
    log::info!("[midi-bench] report: {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lf_engine::{Engine, EngineConfig, ProcessContext, TimedCommand};

    const SPACE: Space = Space { base: 48, notes: 24 };

    #[test]
    fn a_send_s_pair_comes_back_only_after_a_whole_cycle_and_is_read_back_from_the_engine_s_velocity() {
        let cycle = 24 * 127;
        let keys: Vec<Key> = (0..cycle).map(|i| key_of(i, SPACE)).collect();
        let distinct: std::collections::HashSet<Key> = keys.iter().copied().collect();
        assert_eq!(distinct.len(), cycle as usize, "every pair of a cycle distinct");
        assert_eq!(key_of(cycle, SPACE), key_of(0, SPACE));
        assert!(keys.iter().all(|&(n, v)| (48..72).contains(&n) && (1..=127).contains(&v)), "never a velocity 0 (a note-off)");
        for &(note, v) in &keys {
            // The engine gets `v / 127` as f32, from Web MIDI's JSON or the native parse.
            assert_eq!(bench_key(note, v as f32 / 127.0, SPACE), Some((note, v)));
        }
        assert_eq!(bench_key(47, 0.5, SPACE), None, "below the notes: a stray");
        assert_eq!(bench_key(72, 0.5, SPACE), None);
        assert_eq!(bench_key(60, 0.0, SPACE), None);
    }

    fn s(key: Key, at_ms: u64) -> Seen {
        Seen { key, at: at_ms * 1_000_000 }
    }

    #[test]
    fn a_record_matches_the_oldest_send_of_its_pair_a_late_one_is_lost_and_a_spare_one_is_a_duplicate() {
        let (a, b, c) = ((48, 1), (49, 1), (50, 1));
        let sent = [s(a, 0), s(b, 25), s(c, 50), s(a, 5000), s(b, 5025)];
        // b's first send is never applied, and its record turns up 2 s later (past the 1 s timeout): it
        // is lost, and the late record takes b's second send only if that one waits (it does not yet:
        // the record precedes it, so it is a duplicate). c is applied twice.
        let seen = [s(a, 3), s(c, 54), s(c, 60), s(b, 2025), s(a, 5004), s(b, 5030)];
        let m = match_seen(&sent, &seen, 1_000_000_000);
        assert_eq!(m.by_send, vec![Some(0), None, Some(1), Some(4), Some(5)]);
        assert_eq!(m.duplicates, 2, "c's second record and b's late one");
        // A record whose pair was never sent is a duplicate too; order of `seen` does not matter.
        let m = match_seen(&sent, &[s((51, 1), 10), s(a, 3)], 1_000_000_000);
        assert_eq!(m.by_send[0], Some(1));
        assert_eq!(m.duplicates, 1);
    }

    #[test]
    fn percentiles_take_the_nearest_rank() {
        let ms = |v: i64| v * 1_000_000;
        let s = spread((1..=100).map(ms).collect()).unwrap();
        assert_eq!((s.n, s.p50_ms, s.p99_ms, s.max_ms), (100, 50.0, 99.0, 100.0));
        let s = spread((1..=10).rev().map(ms).collect()).unwrap();
        assert_eq!((s.p50_ms, s.p99_ms, s.max_ms), (5.0, 10.0, 10.0), "unsorted in, ranks out");
        let s = spread(vec![1_234_567]).unwrap();
        assert_eq!((s.p50_ms, s.p99_ms, s.max_ms), (1.235, 1.235, 1.235), "rounded to the microsecond");
        assert_eq!(spread(Vec::new()), None);
    }

    fn st(entry_ms: u64, frame: Frame) -> Stamp {
        Stamp { entry_ns: entry_ms * 1_000_000, frame, block: 128, rate: 48_000 }
    }

    #[test]
    fn the_clock_breaks_and_the_blocks_a_note_waited_are_read_from_the_stamps() {
        let stamps = [st(0, 0), st(3, 128), st(6, 256), st(9, 512), Stamp { rate: 0, ..st(10, 0) }, st(20, 640), st(23, 768)];
        assert_eq!(discontinuities(&stamps), 2, "the jump 384 -> 512 and the stop; the restart counts with the stop");
        let running: Vec<Stamp> = stamps.iter().copied().filter(|s| s.rate != 0).collect();
        // Arrived at 1 ms: the block entered at 3 ms should take it.
        assert_eq!(blocks_late(&running, 1_000_000, 128), Some(0));
        assert_eq!(blocks_late(&running, 1_000_000, 256), Some(1));
        // Arrived after the block that applied it entered (pushed between its entry and its take).
        assert_eq!(blocks_late(&running, 3_500_000, 128), Some(0));
        assert_eq!(blocks_late(&running, 1_000_000, 400), None, "a frame no block rendered");
    }

    #[test]
    fn a_run_splits_its_notes_by_the_stall_their_send_fell_in_and_counts_the_lost_and_the_strays() {
        // One block per ms (48 frames at 48 kHz) from frame 0 at t = 100 ms.
        let stamps: Vec<Stamp> = (0..400).map(|k| Stamp { entry_ns: (100 + k) * 1_000_000, frame: 48 * k as Frame, block: 48, rate: 48_000 }).collect();
        let frame_at = |ms: u64| 48 * (ms as Frame - 100);
        let note = |key: Key, applied_ms: u64| AppliedNote { note: key.0, velocity: key.1 as f32 / 127.0, frame: frame_at(applied_ms) };
        let keys: Vec<Key> = (0..4).map(|i| key_of(i, SPACE)).collect();
        let c = Collected {
            sent: vec![s(keys[0], 110), s(keys[1], 150), s(keys[2], 200), s(keys[3], 300)],
            // The second send fell in the stall (140..240 ms) and waited for its end; the third was
            // applied on a frame no block holds (lost); a stray from the player's keyboard (note 30).
            applied: vec![note(keys[0], 113), note(keys[1], 241), AppliedNote { note: 30, velocity: 0.5, frame: frame_at(250) }, note(keys[3], 304), AppliedNote { frame: 48 * 1000, ..note(keys[2], 100) }],
            arrivals: vec![(keys[0].0, keys[0].1 as f32 / 127.0, 112_000_000), (keys[1].0, keys[1].1 as f32 / 127.0, 240_500_000), (keys[3].0, keys[3].1 as f32 / 127.0, 301_200_000)],
            stamps,
            stalls: vec![(140_000_000, 240_000_000)],
        };
        let o = evaluate(&c, SPACE, Duration::from_secs(1));
        assert_eq!((o.applied, o.lost, o.duplicates, o.strays, o.unconverted, o.arrived), (3, 1, 0, 1, 1, 3));
        assert_eq!(o.inside, Side { sent: 2, sender_to_applied: vec![91_000_000], arrival_to_applied: vec![500_000] }, "the lost third counts as sent inside");
        assert_eq!(o.outside, Side { sent: 2, sender_to_applied: vec![3_000_000, 4_000_000], arrival_to_applied: vec![1_000_000, 2_800_000] });
        // Arrived at 301.2 ms, applied in the block that entered at 304: the blocks at 302 and 303 passed it.
        assert_eq!(o.late, vec![0, 0, 2]);
    }

    #[test]
    fn the_config_reads_the_port_and_its_keys_and_refuses_what_cannot_run() {
        let c = Config::parse("loopMIDI Port; interval_ms=20; notes=12; burst=3; count=500; warmup_s=2; stall_every_ms=3000; stall_ms=300; label=before").unwrap();
        assert_eq!((c.port.as_str(), c.interval, c.notes, c.burst, c.count, c.warmup), ("loopMIDI Port", Duration::from_millis(20), 12, 3, 500, Duration::from_secs(2)));
        assert_eq!(c.stalls, Some(StallPlan { every_ms: 3000, ms: 300 }));
        assert_eq!(c.label, "before");
        let d = Config::parse("port=bench").unwrap();
        assert_eq!((d.interval, d.notes, d.base, d.burst, d.count, d.stalls), (Duration::from_millis(25), 24, 48, 1, 2000, None));
        for bad in ["", "interval_ms=20", "x;interval_ms=1", "x;notes=0", "x;base=120;notes=24", "x;burst=25", "x;count=0", "x;stall_every_ms=100;stall_ms=100", "x;speed=3", "x;count=many"] {
            assert!(Config::parse(bad).is_err(), "{bad:?} parsed");
        }
    }

    /// The engine records each applied `NoteOn` without allocating on the audio path: its frame is the
    /// block start an unstamped command lands on; a full record refuses and counts, still allocating
    /// nothing. `host::rt_alloc` is the test build's global allocator.
    #[test]
    fn the_engine_records_each_applied_note_on_and_its_frame_without_allocating() {
        use crate::host::rt_alloc;
        let before = rt_alloc::allocations();
        {
            let _g = rt_alloc::guard();
            std::hint::black_box(vec![0u8; 16]);
        }
        assert!(rt_alloc::allocations() > before, "the allocation counter is not live in this build");

        let (mut engine, mut handle) = Engine::new(EngineConfig { max_loop_seconds: 1.0, ..EngineConfig::new(48_000) });
        let mut reader = engine.take_applied_notes().expect("the reader");
        assert!(engine.take_applied_notes().is_none(), "handed out once");
        let input = vec![0.0f32; 128];
        let (mut l, mut r) = (vec![0.0f32; 128], vec![0.0f32; 128]);
        let mut frame = 0;
        let mut block = |engine: &mut Engine, frame: &mut Frame| {
            let ctx = ProcessContext { frame: *frame, xrun: false, damaged: false, align_frames: 0, input_frames: 0 };
            let _g = rt_alloc::guard();
            engine.process(&ctx, &input, &mut l, &mut r);
            *frame += 128;
        };
        block(&mut engine, &mut frame);
        handle.commands.push(TimedCommand { frame: None, command: Command::NoteOn(60, 0.5) }).unwrap();
        handle.commands.push(TimedCommand { frame: None, command: Command::NoteOff(60) }).unwrap();
        handle.commands.push(TimedCommand { frame: None, command: Command::NoteOn(61, 1.0) }).unwrap();
        let allocated = rt_alloc::allocations();
        block(&mut engine, &mut frame);
        block(&mut engine, &mut frame);
        assert_eq!(rt_alloc::allocations(), allocated, "the apply path allocated");
        let notes: Vec<AppliedNote> = std::iter::from_fn(|| reader.rx.pop().ok()).collect();
        assert_eq!(notes, vec![AppliedNote { note: 60, velocity: 0.5, frame: 128 }, AppliedNote { note: 61, velocity: 1.0, frame: 128 }]);

        // Fill it past its capacity, 60 notes a block (under the engine's 64-command table), undrained.
        let blocks = lf_engine::note_record::CAPACITY / 60 + 2;
        let allocated = rt_alloc::allocations();
        for b in 0..blocks {
            for k in 0..60 {
                handle.commands.push(TimedCommand { frame: None, command: Command::NoteOn((b * 60 + k) as u8 % 128, 0.25) }).unwrap();
            }
            block(&mut engine, &mut frame);
        }
        assert_eq!(rt_alloc::allocations(), allocated, "a full record allocated");
        assert_eq!(reader.refused(), (blocks * 60 - lf_engine::note_record::CAPACITY) as u64);
        assert_eq!(reader.rx.slots(), lf_engine::note_record::CAPACITY);
    }
}
