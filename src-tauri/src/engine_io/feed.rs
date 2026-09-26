//! OWNS: the feed (`docs/plans/native-engine.md` § Stage 5, Wire): what the UI reads back from the
//! engine, built on its own non-RT thread about 60 times a second ([`FeedFrame`]). Each tick drains the
//! engine's events and the device's, and reads the device status, the clock anchor, the input meter
//! and the lanes' waveforms; a frame goes out when any of them changed, and at least every `REFRESH`
//! while a device runs (the UI extrapolates its playhead from the anchor between frames). Never PCM.
//!
//! The feed is the only reader of the engine's event ring and of the device events, so it keeps a
//! mirror of the lanes, the transport and the selection: a new subscriber (a WebView reload) and a
//! new engine (another sample rate, a fault) get a `reset` frame that carries all of it. The thread
//! drains while nobody subscribes, so the event ring never fills.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lf_engine::grid::Frame;
use lf_engine::overview::PEAK_FRAMES;
use lf_engine::{Event, LaneInfo, LaneState, Overview, TRACK_COUNT};

use super::wire::{ClockAnchor, FeedFrame, Meter, PeakUpdate, WireEvent};
use super::{DeviceStatus, EngineHost};

/// The feed's period: about 60 frames a second.
const TICK: Duration = Duration::from_micros(16_667);
/// The longest a running device goes without a frame (the anchor's refresh).
const REFRESH: Duration = Duration::from_millis(500);

/// A lane no engine has reported: what a new engine starts with.
const EMPTY_LANE: LaneInfo = LaneInfo {
    state: LaneState::Empty,
    length: 0,
    armed: false,
    auto_armed: false,
    can_undo: false,
    can_reverse: false,
    reversed: false,
    stop_at: None,
    retake_pass: 0,
};

/// What the UI was last sent of a lane's waveform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Drawn {
    buf: usize,
    reversed: bool,
    count: u32,
}

/// One tick's worth of the feed, kept between ticks.
pub(crate) struct Feed {
    host: EngineHost,
    seq: u64,
    /// The engine the mirror follows (`Core::engine_gen`), and its overview.
    gen: Option<u64>,
    overview: Option<Arc<Overview>>,
    lanes: [Option<(Frame, LaneInfo)>; TRACK_COUNT],
    transport: Option<Event>,
    selected: (Frame, u8),
    /// What the last frames said.
    status: Option<DeviceStatus>,
    meter: Option<Meter>,
    sent: Option<Instant>,
    drained: Vec<Event>,
    /// Each lane's waveform as last sent (`None`: nothing yet, or a reset since).
    drawn: [Option<Drawn>; TRACK_COUNT],
    bins: Vec<u32>,
}

impl Feed {
    pub(crate) fn new(host: EngineHost) -> Feed {
        Feed {
            host,
            seq: 0,
            gen: None,
            overview: None,
            lanes: [None; TRACK_COUNT],
            transport: None,
            selected: (0, 0),
            status: None,
            meter: None,
            sent: None,
            drained: Vec::with_capacity(256),
            drawn: [None; TRACK_COUNT],
            bins: Vec::new(),
        }
    }

    /// Read everything once; the frame to send, if anything changed or the anchor is due. `reset`: a
    /// new subscriber, who gets the whole state.
    pub(crate) fn tick(&mut self, mut reset: bool) -> Option<FeedFrame> {
        self.drained.clear();
        let (gen, overview) = self.host.drain_feed(&mut self.drained);
        if self.gen != Some(gen) {
            // A new engine (or none): what it reports starts from EMPTY lanes and no transport.
            reset |= self.gen.is_some();
            self.gen = Some(gen);
            self.overview = overview;
            self.lanes = [None; TRACK_COUNT];
            self.transport = None;
            self.selected = (0, 0);
        }
        for event in &self.drained {
            match *event {
                Event::Lane { frame, lane, info } if usize::from(lane) < TRACK_COUNT => self.lanes[usize::from(lane)] = Some((frame, info)),
                Event::Transport { .. } => self.transport = Some(*event),
                Event::Selected { frame, lane } => self.selected = (frame, lane),
                Event::Copied { from, to, .. } => self.host.copied(from, to),
                _ => {}
            }
        }
        let device = self.host.take_device_events();
        let now = self.host.status();
        let status = (reset || now != self.status).then(|| now.clone());
        self.status = now;
        let running = self.status.is_some();
        let anchor = if running { self.anchor() } else { None };
        let meter = running.then(|| {
            let (peak, clip) = self.host.take_meter();
            Meter { peak, clip }
        });
        let metered = meter != self.meter;
        self.meter = meter;
        let peaks = self.peaks(reset);
        let due = running && self.sent.is_none_or(|at| at.elapsed() >= REFRESH);
        if !(reset || due || metered || status.is_some() || !self.drained.is_empty() || !device.is_empty() || !peaks.is_empty()) {
            return None;
        }
        let events = if reset { self.state_events() } else { self.drained.iter().copied().map(WireEvent).collect() };
        self.sent = Some(Instant::now());
        self.seq += 1;
        Some(FeedFrame { seq: self.seq - 1, reset, events, device, status, anchor, meter, peaks })
    }

    /// The lanes' waveform bins that changed, in play order: every bin of a lane that shows another
    /// buffer or orientation, holds less than before, or is new to the UI (a reset), else the bins its
    /// buffer changed plus the ones it grew by.
    fn peaks(&mut self, reset: bool) -> Vec<PeakUpdate> {
        let mut out = Vec::new();
        let Some(overview) = self.overview.clone() else {
            self.drawn = [None; TRACK_COUNT];
            return out;
        };
        let total = overview.bins();
        for lane in 0..TRACK_COUNT {
            let view = overview.lane(lane);
            let count = (view.frames.max(0) as usize).div_ceil(PEAK_FRAMES).min(total) as u32;
            let now = Drawn { buf: view.buf, reversed: view.reversed, count };
            let last = if reset { None } else { self.drawn[lane] };
            self.drawn[lane] = Some(now);
            self.bins.clear();
            match last {
                Some(d) if d.buf == now.buf && d.reversed == now.reversed && count >= d.count => {
                    let bins = &mut self.bins;
                    overview.take_dirty(view.buf, |b| {
                        if (b as u32) < count {
                            bins.push(b as u32);
                        }
                    });
                    bins.extend(d.count..count);
                }
                _ => {
                    overview.clear_dirty(view.buf);
                    if count == 0 {
                        // Cleared (or a pass starting over): one empty update, none for a lane never drawn.
                        if last.is_some_and(|d| d.count > 0) {
                            out.push(PeakUpdate { lane: lane as u8, start: 0, count: 0, min: Vec::new(), max: Vec::new() });
                        }
                        continue;
                    }
                    self.bins.extend(0..count);
                }
            }
            if now.reversed {
                for b in self.bins.iter_mut() {
                    *b = count - 1 - *b;
                }
            }
            self.bins.sort_unstable();
            self.bins.dedup();
            // Contiguous runs of play bins, one update each.
            let mut k = 0;
            while k < self.bins.len() {
                let start = self.bins[k];
                let mut end = k + 1;
                while end < self.bins.len() && self.bins[end] == self.bins[end - 1] + 1 {
                    end += 1;
                }
                let (mut min, mut max) = (Vec::with_capacity(end - k), Vec::with_capacity(end - k));
                for &play in &self.bins[k..end] {
                    let bin = if now.reversed { count - 1 - play } else { play };
                    let (lo, hi) = overview.bin(view.buf, bin as usize);
                    min.push(round(lo));
                    max.push(round(hi));
                }
                out.push(PeakUpdate { lane: lane as u8, start, count, min, max });
                k = end;
            }
        }
        out
    }

    fn anchor(&self) -> Option<ClockAnchor> {
        let (frame, at_ms, rate) = self.host.core.clock.anchor()?;
        let grid = self.overview.as_ref().map_or(0, |o| o.grid());
        Some(ClockAnchor { frame, at_ms, rate, grid })
    }

    /// A reset frame's events: the transport (once an engine reported it), every lane, the selected
    /// lane, then what else this tick drained (a beat, a refusal, a copy).
    fn state_events(&self) -> Vec<WireEvent> {
        let lanes = self.lanes.iter().enumerate().map(|(i, lane)| {
            let (frame, info) = lane.unwrap_or((0, EMPTY_LANE));
            Event::Lane { frame, lane: i as u8, info }
        });
        let selected = Event::Selected { frame: self.selected.0, lane: self.selected.1 };
        let rest = self.drained.iter().copied().filter(|e| !matches!(e, Event::Lane { .. } | Event::Transport { .. } | Event::Selected { .. }));
        self.transport.into_iter().chain(lanes).chain([selected]).chain(rest).map(WireEvent).collect()
    }
}

/// A bin's value as sent: three decimals are plenty for a waveform and keep the JSON short.
fn round(v: f32) -> f32 {
    ((v as f64 * 1000.0).round() / 1000.0) as f32
}

/// Where frames go: false when the subscriber is gone.
type Sink = Box<dyn FnMut(FeedFrame) -> bool + Send>;

struct Subscriber {
    send: Sink,
    /// Has not had its reset frame yet.
    fresh: bool,
}

/// The feed thread and its one subscriber: a new subscribe replaces the last (a WebView reload
/// subscribes again; the old document's channel is gone with it).
pub(crate) struct FeedThread {
    subscriber: Arc<Mutex<Option<Subscriber>>>,
    stop: Arc<AtomicBool>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl FeedThread {
    pub(crate) fn spawn(host: EngineHost) -> std::io::Result<FeedThread> {
        let subscriber: Arc<Mutex<Option<Subscriber>>> = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let (sub, halt) = (subscriber.clone(), stop.clone());
        let join = std::thread::Builder::new().name("lf-engine-feed".into()).spawn(move || {
            let mut feed = Feed::new(host);
            let mut next = Instant::now();
            while !halt.load(Relaxed) {
                {
                    // One lock for the tick and its send: a subscriber's first frame is its reset.
                    let mut sub = sub.lock().unwrap_or_else(|e| e.into_inner());
                    let fresh = sub.as_mut().is_some_and(|s| std::mem::take(&mut s.fresh));
                    if let (Some(frame), Some(s)) = (feed.tick(fresh), sub.as_mut()) {
                        if !(s.send)(frame) {
                            log::warn!("[engine_io] the feed's subscriber is gone");
                            *sub = None;
                        }
                    }
                }
                next += TICK;
                let now = Instant::now();
                if next > now {
                    std::thread::sleep(next - now);
                } else {
                    next = now;
                }
            }
        })?;
        Ok(FeedThread { subscriber, stop, join: Mutex::new(Some(join)) })
    }

    /// Send every frame to `send` from now on (its first is a reset), instead of the last subscriber.
    pub(crate) fn subscribe(&self, send: impl FnMut(FeedFrame) -> bool + Send + 'static) {
        *self.subscriber.lock().unwrap_or_else(|e| e.into_inner()) = Some(Subscriber { send: Box::new(send), fresh: true });
    }

    /// Stop the thread and wait for it.
    pub(crate) fn stop(&self) {
        self.stop.store(true, Relaxed);
        if let Some(join) = self.join.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = join.join();
        }
    }
}
