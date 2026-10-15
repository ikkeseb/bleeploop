//! OWNS: the feed (its frame: `wire.rs`): what the UI reads back from the
//! engine, built on its own non-RT thread about 60 times a second ([`FeedFrame`]). Each tick drains the
//! engine's events and the device's, and reads the device status, the clock anchor, the input meter
//! and the lanes' waveforms; a frame goes out when any of them changed, and at least every `REFRESH`
//! while a device runs (the UI extrapolates its playhead from the anchor between frames). Never PCM.
//!
//! The scope taps (`lf_engine::scope`) are drained every tick whatever the frame carries, held or not:
//! their ring left to fill would make the next batch a replay of seconds of old sound as a fast light
//! show. A batch the UI cannot splice onto what it holds (columns the engine or a held tick lost, the
//! cap, a new engine, a reset frame) carries `gap`, and the UI drops its trace instead.
//!
//! The feed is the only reader of the engine's event ring and of the device events, but for a rebuild,
//! which drains the replaced engine's ring into the settings memory (`owner.rs` `swap_engine`). So it
//! keeps a mirror of the lanes, the transport and the selection: a new subscriber (a WebView reload) and
//! a new engine (another sample rate, a fault) get a `reset` frame that carries all of it, with the
//! settings the host keeps and each lane's last `Mix` (`settings.rs`, which every drain feeds), so the
//! UI adopts them. The thread drains while nobody subscribes, so the event ring never fills; it then
//! ticks as a held feed does (below): no frame is built, and the device events wait for the next
//! subscriber's reset.
//!
//! The feed can be HELD (`FeedThread::hold`): while the UI thread cannot read (the native folder
//! dialog is modal on it), every frame sent would wait in the channel, with no bound, and arrive in
//! one burst. A held feed keeps ticking, so the ring is drained and the mirror stays true, but builds
//! and sends nothing; its first tick after the hold is one `reset` frame, which carries the whole
//! state. `seq` counts frames built, so it does not move while held. The UI reads `seq` for nothing
//! and replaces its view on a reset (`applyFrameNow`, `src/ui/state/engine-store.ts`), so the resync
//! is no fault to it. What only an ordinary frame carries (a beat, a refusal) is not replayed, as
//! for a WebView reload; device events are kept and go out with the reset.

use std::sync::atomic::{
    AtomicBool, AtomicUsize,
    Ordering::{AcqRel, Acquire, Relaxed, Release},
};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lf_engine::grid::Frame;
use lf_engine::overview::PEAK_FRAMES;
use lf_engine::scope::{ScopeBin, SCOPE_SOURCES};
use lf_engine::{Event, LaneInfo, LaneState, Overview, TRACK_COUNT};

use super::wire::{ClockAnchor, FeedFrame, Meter, PeakUpdate, ScopeUpdate, WireCommand, WireEvent};
use super::{DeviceEvent, DeviceStatus, EngineHost};

/// The device events a held feed keeps for its reset frame; older ones go first.
const HELD_DEVICE_EVENTS: usize = 64;

/// The most scope columns one frame carries: a tick is about four 4 ms columns, so this is a quarter
/// second of them — slack for a tick that ran late or a device that woke in bursts. Past it the oldest
/// go and the UI is told its trace broke (`gap`): a batch the size of the whole ring would play seconds
/// of old sound as a fast light show.
const SCOPE_BATCH: usize = 64;

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
    fading: false,
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
    /// The device events taken while the feed was held: the next frame carries them first.
    held_device: Vec<DeviceEvent>,
    /// This tick's scope columns, reused between ticks, and the frames one column of the engine they
    /// came from covers (0: no engine, so no columns).
    columns: Vec<ScopeBin>,
    scope_bin: u32,
    /// The device frame the next column must start at to splice onto the last batch sent; `None`
    /// before the first batch.
    scope_next: Option<Frame>,
    /// Columns were lost since the last batch went out (a full ring, a skip, a held feed, the cap, a new
    /// engine): the next batch says so and the UI drops the trace it holds.
    scope_gap: bool,
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
            held_device: Vec::new(),
            columns: Vec::with_capacity(lf_engine::scope::SCOPE_CAPACITY),
            scope_bin: 0,
            scope_next: None,
            scope_gap: false,
        }
    }

    /// Drain the engine's events into the mirror. True when another engine replaced the one the
    /// mirror followed: the UI must reset.
    fn drain(&mut self) -> bool {
        let mut replaced = false;
        self.drained.clear();
        self.columns.clear();
        // The events and the columns in one acquisition, so this tick's columns are the engine whose
        // events it took: a second drain could straddle a rebuild and mix the two.
        let (gen, overview, bin) = self.host.drain_feed_scope(&mut self.drained, Some(&mut self.columns));
        self.scope_bin = bin;
        if self.gen != Some(gen) {
            // A new engine (or none): what it reports starts from EMPTY lanes and no transport.
            replaced = self.gen.is_some();
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
                _ => {}
            }
        }
        replaced
    }

    /// A tick of a held feed, or of one nobody subscribed to: the ring is drained and the mirror follows
    /// it, the device events are kept for the next frame (the latest `HELD_DEVICE_EVENTS`), and no frame
    /// is built (`seq` stays). The reset that ends the hold, or a new subscriber's, reads the status, the
    /// meter and the waveforms afresh.
    pub(crate) fn tick_held(&mut self) {
        self.drain();
        self.scope(false);
        self.held_device.extend(self.host.take_device_events());
        // A device that keeps failing behind a dialog left open: the latest are what the page needs.
        let over = self.held_device.len().saturating_sub(HELD_DEVICE_EVENTS);
        self.held_device.drain(..over);
    }

    /// Read everything once; the frame to send, if anything changed or the anchor is due. `reset`: a
    /// new subscriber or the end of a hold: the frame carries the whole state.
    pub(crate) fn tick(&mut self, mut reset: bool) -> Option<FeedFrame> {
        reset |= self.drain();
        let mut device = std::mem::take(&mut self.held_device);
        device.extend(self.host.take_device_events());
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
        // A reset carries no scope: the UI replaces its view with this frame, so the trace starts over.
        let scope = self.scope(!reset);
        let due = running && self.sent.is_none_or(|at| at.elapsed() >= REFRESH);
        // A batch of columns is itself a reason to send: a tick that built one never drops it.
        if !(reset || due || metered || status.is_some() || !self.drained.is_empty() || !device.is_empty() || !peaks.is_empty() || scope.is_some()) {
            return None;
        }
        let events = if reset { self.state_events() } else { self.drained.iter().copied().map(WireEvent).collect() };
        let settings = reset.then(|| self.host.settings().into_iter().map(WireCommand).collect());
        self.sent = Some(Instant::now());
        self.seq += 1;
        Some(FeedFrame { seq: self.seq - 1, reset, events, device, status, anchor, meter, peaks, scope, settings })
    }

    /// Build this frame's batch from the columns `drain` took. `send` false — held, nobody subscribed,
    /// or a reset frame — drops them and leaves the UI's trace broken (`gap`). What goes out is one
    /// contiguous run: a hole inside the drain (a device-frame skip, a ring the engine overran, the
    /// taps switched off and on between two ticks) cannot be sent as one batch, so only the newest run
    /// is, and the batch is capped at `SCOPE_BATCH`, past which the oldest of that run go. `None`: no
    /// columns, so the frame carries no scope.
    fn scope(&mut self, send: bool) -> Option<ScopeUpdate> {
        let bin = self.scope_bin as Frame;
        if !send || bin == 0 {
            self.scope_gap |= !self.columns.is_empty();
            self.scope_next = None;
            return None;
        }
        // The last run whose columns follow each other, then the cap: everything before it is lost
        // work the UI must not splice over.
        let mut from = 0;
        for k in 1..self.columns.len() {
            if self.columns[k].frame != self.columns[k - 1].frame + bin {
                from = k;
            }
        }
        let over = (self.columns.len() - from).saturating_sub(SCOPE_BATCH);
        let lost = from + over > 0;
        self.columns.drain(..from + over);
        let first = *self.columns.first()?;
        // A batch that does not follow the last one sent: the engine lost columns, this drain had a
        // hole, or the cap took some, so the UI starts its trace over here.
        let gap = std::mem::take(&mut self.scope_gap) || lost || self.scope_next != Some(first.frame);
        self.scope_next = Some(first.frame + self.columns.len() as Frame * bin);
        let columns = self.columns.len();
        let (mut min, mut max) = (Vec::with_capacity(SCOPE_SOURCES), Vec::with_capacity(SCOPE_SOURCES));
        for source in 0..SCOPE_SOURCES {
            min.push(self.columns.iter().map(|c| round(c.lo[source])).collect::<Vec<f32>>());
            max.push(self.columns.iter().map(|c| round(c.hi[source])).collect::<Vec<f32>>());
        }
        debug_assert!(min.iter().chain(&max).all(|c| c.len() == columns));
        Some(ScopeUpdate { frame: first.frame, bin: self.scope_bin, gap, min, max })
    }

    /// The lanes' waveform bins that changed, in play order: every bin of a lane that shows another
    /// buffer or orientation, holds less than before, grows while reversed (a multiply: its play order
    /// counts back from the new end) or is new to the UI (a reset), else the bins its buffer changed plus
    /// the ones it grew by. Each run lists its marked bins (`Overview::heard_marked`): the peaks keep
    /// their bin reversal, the marks map through frames, so a changed buffer bin on a reversed lane also
    /// re-sends the heard bins its frames reach (at most two).
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
                Some(d) if d.buf == now.buf && d.reversed == now.reversed && (count == d.count || (count > d.count && !now.reversed)) => {
                    let (bins, frames) = (&mut self.bins, view.frames.max(0) as usize);
                    overview.take_dirty(view.buf, |b| {
                        if (b as u32) < count {
                            bins.push(b as u32);
                            if now.reversed {
                                // Pushed in buffer order, reversed below: the heard bins, reversed back.
                                if let Some((lo, hi)) = Overview::heard_bins(frames, true, b) {
                                    bins.extend((lo..=hi).filter(|&h| (h as u32) < count).map(|h| count - 1 - h as u32));
                                }
                            }
                        }
                    });
                    bins.extend(d.count..count);
                }
                _ => {
                    overview.clear_dirty(view.buf);
                    if count == 0 {
                        // Cleared (or a pass starting over): one empty update, none for a lane never drawn.
                        if last.is_some_and(|d| d.count > 0) {
                            out.push(PeakUpdate { lane: lane as u8, start: 0, count: 0, min: Vec::new(), max: Vec::new(), marked: Vec::new() });
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
                let (mut min, mut max, mut marked) = (Vec::with_capacity(end - k), Vec::with_capacity(end - k), Vec::new());
                let frames = view.frames.max(0) as usize;
                for &play in &self.bins[k..end] {
                    let bin = if now.reversed { count - 1 - play } else { play };
                    let (lo, hi) = overview.bin(view.buf, bin as usize);
                    min.push(round(lo));
                    max.push(round(hi));
                    if overview.heard_marked(view.buf, frames, now.reversed, play as usize) {
                        marked.push(play);
                    }
                }
                out.push(PeakUpdate { lane: lane as u8, start, count, min, max, marked });
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

    /// A reset frame's events: the transport (once an engine reported it), every lane, each lane's last
    /// `Mix` the host kept (a lane no engine has reported has none), the selected lane, then what else this
    /// tick drained (a beat, a refusal, a copy).
    fn state_events(&self) -> Vec<WireEvent> {
        let lanes = self.lanes.iter().enumerate().map(|(i, lane)| {
            let (frame, info) = lane.unwrap_or((0, EMPTY_LANE));
            Event::Lane { frame, lane: i as u8, info }
        });
        let selected = Event::Selected { frame: self.selected.0, lane: self.selected.1 };
        let rest = self.drained.iter().copied().filter(|e| !matches!(e, Event::Lane { .. } | Event::Transport { .. } | Event::Selected { .. } | Event::Mix { .. }));
        self.transport.into_iter().chain(lanes).chain(self.host.mixes()).chain([selected]).chain(rest).map(WireEvent).collect()
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

/// What holds the feed's sends: how many guards are out, and that one was dropped since the thread
/// last sent (the UI missed frames, or could have: it gets a reset).
#[derive(Default)]
struct Hold {
    holders: AtomicUsize,
    resync: AtomicBool,
}

/// The feed sends nothing while one of these lives (`FeedThread::hold`). Its drop ends the hold on
/// every path out of its scope, and the feed's next tick sends one reset frame.
pub(crate) struct FeedHold(Arc<Hold>);

impl FeedHold {
    fn take(hold: &Arc<Hold>) -> FeedHold {
        hold.holders.fetch_add(1, AcqRel);
        FeedHold(hold.clone())
    }
}

impl Drop for FeedHold {
    fn drop(&mut self) {
        // The resync first: a tick that sees no holder left always sees it.
        self.0.resync.store(true, Release);
        self.0.holders.fetch_sub(1, AcqRel);
    }
}

/// One tick of the feed thread, under the subscriber's lock. Held, or with nobody subscribed: the feed
/// ticks as a held one and builds nothing (a subscriber that came while held gets nothing either: its
/// reset waits for the hold's end). Else one frame at most goes out, a reset when the subscriber is new
/// or a hold has ended since the last tick.
fn turn(feed: &mut Feed, subscriber: &mut Option<Subscriber>, hold: &Hold) {
    if hold.holders.load(Acquire) > 0 || subscriber.is_none() {
        feed.tick_held();
        return;
    }
    let resync = hold.resync.swap(false, AcqRel);
    let fresh = subscriber.as_mut().is_some_and(|s| std::mem::take(&mut s.fresh));
    if let (Some(frame), Some(s)) = (feed.tick(fresh || resync), subscriber.as_mut()) {
        if !(s.send)(frame) {
            log::warn!("[engine_io] the feed's subscriber is gone");
            *subscriber = None;
        }
    }
}

/// The feed thread and its one subscriber: a new subscribe replaces the last (a WebView reload
/// subscribes again; the old document's channel is gone with it).
pub(crate) struct FeedThread {
    subscriber: Arc<Mutex<Option<Subscriber>>>,
    hold: Arc<Hold>,
    stop: Arc<AtomicBool>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl FeedThread {
    pub(crate) fn spawn(host: EngineHost) -> std::io::Result<FeedThread> {
        let subscriber: Arc<Mutex<Option<Subscriber>>> = Arc::new(Mutex::new(None));
        let hold = Arc::new(Hold::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (sub, held, halt) = (subscriber.clone(), hold.clone(), stop.clone());
        let join = std::thread::Builder::new().name("lf-engine-feed".into()).spawn(move || {
            let mut feed = Feed::new(host);
            let mut next = Instant::now();
            while !halt.load(Relaxed) {
                {
                    // One lock for the tick and its send: a subscriber's first frame is its reset.
                    let mut sub = sub.lock().unwrap_or_else(|e| e.into_inner());
                    turn(&mut feed, &mut sub, &held);
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
        Ok(FeedThread { subscriber, hold, stop, join: Mutex::new(Some(join)) })
    }

    /// Send every frame to `send` from now on (its first is a reset), instead of the last subscriber.
    pub(crate) fn subscribe(&self, send: impl FnMut(FeedFrame) -> bool + Send + 'static) {
        *self.subscriber.lock().unwrap_or_else(|e| e.into_inner()) = Some(Subscriber { send: Box::new(send), fresh: true });
    }

    /// Hold the feed's sends until the guard drops (the module doc says why and what the UI gets
    /// after). Taken under the subscriber's lock, which a tick holds through its send: once this
    /// returns, no frame is on its way out. The thread keeps ticking, and `stop` still stops it.
    pub(crate) fn hold(&self) -> FeedHold {
        let _no_send_in_flight = self.subscriber.lock().unwrap_or_else(|e| e.into_inner());
        FeedHold::take(&self.hold)
    }

    /// Stop the thread and wait for it.
    pub(crate) fn stop(&self) {
        self.stop.store(true, Relaxed);
        if let Some(join) = self.join.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = join.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine_io::test_rig::TestDevice;
    use lf_engine::{Command, LaneState, ProcessContext, TimedCommand};

    type Sent = Arc<Mutex<Vec<FeedFrame>>>;

    /// An engine a plain thread renders (no device, so the feed has no status, anchor or meter to
    /// send: a frame goes out only for a reset or an engine event), once it has reported its start.
    fn engine() -> TestDevice {
        let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
        assert!(device.wait_blocks(4, Duration::from_secs(5)), "the engine renders");
        device
    }

    /// A subscriber that has not had its reset, and the frames it is sent.
    fn subscriber() -> (Option<Subscriber>, Sent) {
        let sent = Sent::default();
        let sink = sent.clone();
        let send: Sink = Box::new(move |frame| {
            sink.lock().unwrap().push(frame);
            true
        });
        (Some(Subscriber { send, fresh: true }), sent)
    }

    /// Change the tempo and wait until the engine has run the command: its transport event is in the ring.
    fn set_bpm(device: &TestDevice, bpm: f64) {
        device.host().send(TimedCommand { frame: None, command: Command::SetBpm(bpm) }).unwrap();
        assert!(device.wait_blocks(4, Duration::from_secs(5)), "the engine renders");
    }

    fn tempo(frame: &FeedFrame) -> Option<u32> {
        frame.events.iter().find_map(|e| match e.0 {
            Event::Transport { bpm, .. } => Some(bpm),
            _ => None,
        })
    }

    /// How many frames were sent, and how many of them were resets.
    fn counts(sent: &Sent) -> (usize, usize) {
        let sent = sent.lock().unwrap();
        (sent.len(), sent.iter().filter(|frame| frame.reset).count())
    }

    /// An engine rendered here, block by block on the test's thread: the frames, the device's flags
    /// and the input are the test's own, so a dropout lands where the test puts it.
    struct Manual {
        host: EngineHost,
        frame: Frame,
        input: fn(Frame) -> f32,
    }

    impl Manual {
        /// 4-second lanes at 48 kHz, the event ring `events` long.
        fn new(events: usize) -> Manual {
            use lf_engine::{Engine, EngineConfig};
            let core = Arc::new(super::super::Core::new());
            let config = EngineConfig { max_loop_seconds: 4.0, event_capacity: events, ..EngineConfig::new(48_000) };
            let (engine, handle) = Engine::new(config);
            let [p0, p1] = handle.slots;
            *core.ports[0].lock().unwrap() = Some(p0);
            *core.ports[1].lock().unwrap() = Some(p1);
            let scope_bin = lf_engine::scope::scope_bin_frames(48_000) as u32;
            *core.ends.lock().unwrap() = Some(super::super::Ends {
                commands: handle.commands,
                events: handle.events,
                overview: handle.overview,
                session: Some(handle.session),
                scope: handle.scope,
                scope_bin,
            });
            core.rt.lock().unwrap().engine = Some(engine);
            core.rate.store(48_000, Relaxed);
            core.max_block.store(config.max_block as u32, Relaxed);
            core.running.store(true, Release);
            let manual = Manual { host: EngineHost { core }, frame: 0, input: |f| ((f % 977) as f32 / 977.0) * 0.5 + 0.1 };
            manual.send(Command::SetSlotLive(0, true));
            manual
        }

        fn send(&self, command: Command) {
            self.host.send(TimedCommand { frame: None, command }).unwrap();
        }

        /// Render `frames` in 128-frame blocks, the first flagged as `xrun` and every one as `damaged`.
        fn render(&mut self, frames: Frame, xrun: bool, damaged: bool) {
            let mut rt = self.host.core.rt.lock().unwrap();
            let engine = rt.engine.as_mut().unwrap();
            let (mut x, mut l, mut r) = ([0.0f32; 128], [0.0f32; 128], [0.0f32; 128]);
            let end = self.frame + frames;
            let mut first = true;
            while self.frame < end {
                let n = (end - self.frame).min(128) as usize;
                for (k, s) in x[..n].iter_mut().enumerate() {
                    *s = if damaged { 0.0 } else { (self.input)(self.frame + k as Frame) };
                }
                let ctx = ProcessContext { frame: self.frame, xrun: xrun && first, damaged, align_frames: 0, input_frames: 0 };
                engine.process(&ctx, &x[..n], &mut l[..n], &mut r[..n]);
                self.frame += n as Frame;
                first = false;
            }
        }

        /// The device skips `frames`: the next block follows a gap.
        fn jump(&mut self, frames: Frame) {
            self.frame += frames;
            self.render(128, true, false);
        }

        /// Lane `lane`'s view as the overview holds it.
        fn view(&self, lane: usize) -> lf_engine::LaneView {
            self.host.core.ends.lock().unwrap().as_ref().unwrap().overview.lane(lane)
        }
    }

    /// The marked play bins of lane `lane` that `frames` sent, last update per bin winning; `None` for a
    /// bin no update named.
    fn sent_marks(frames: &[FeedFrame], lane: u8) -> std::collections::BTreeMap<u32, bool> {
        let mut out = std::collections::BTreeMap::new();
        for f in frames {
            if f.reset {
                out.clear();
            }
            for p in f.peaks.iter().filter(|p| p.lane == lane) {
                for b in p.start..p.start + p.min.len() as u32 {
                    out.insert(b, p.marked.contains(&b));
                }
            }
        }
        out
    }

    fn marked(frames: &[FeedFrame], lane: u8) -> Vec<u32> {
        sent_marks(frames, lane).into_iter().filter(|&(_, m)| m).map(|(b, _)| b).collect()
    }

    /// A 2-bar FIXED take at 240 BPM on lane 0 (96,000 frames: 93.75 bins), a damaged block over frames
    /// [48_000, 48_128) of it: buffer bin 46.
    fn take_with_a_dropout(m: &mut Manual) {
        for c in [Command::SetBpm(240.0), Command::SetFixedLength(true), Command::SetFixedBars(2.0), Command::RecDub(0)] {
            m.send(c);
        }
        m.render(128, false, false);
        let window = m.host.core.rt.lock().unwrap().engine.as_ref().unwrap().looper().recorder();
        let Some((0, Some(s), Some(e))) = window else { panic!("no window: {window:?}") };
        m.render(s + 48_000 - m.frame, false, false);
        m.render(128, false, true);
        m.render(e + 512 - m.frame, false, false);
        assert_eq!(m.view(0).state, LaneState::Playing, "the take committed");
    }

    #[test]
    fn a_mark_reaches_the_feed_through_a_full_event_ring_and_a_reset() {
        let mut m = Manual::new(8);
        let mut feed = Feed::new(m.host.clone());
        let hold = Arc::new(Hold::default());
        let (mut sub, sent) = subscriber();
        turn(&mut feed, &mut sub, &hold);
        // The whole take renders with no tick: the ring of 8 overflows and the Dropout is lost.
        take_with_a_dropout(&mut m);
        turn(&mut feed, &mut sub, &hold);
        {
            let sent = sent.lock().unwrap();
            let last = sent.last().unwrap();
            assert!(!last.reset, "an ordinary frame");
            assert!(!last.events.iter().any(|e| matches!(e.0, Event::Dropout { .. })), "the full ring lost the Dropout");
            assert_eq!(marked(&sent, 0), [46], "the mark arrives with the peaks all the same");
        }
        // A reset (a reload's resubscribe) sends it again.
        let (mut sub, sent) = subscriber();
        turn(&mut feed, &mut sub, &hold);
        assert_eq!(marked(&sent.lock().unwrap(), 0), [46], "a resubscribe re-sends the mark");
        // A held feed resumes with one reset that carries it.
        let guard = FeedHold::take(&hold);
        m.render(1024, false, false);
        turn(&mut feed, &mut sub, &hold);
        drop(guard);
        turn(&mut feed, &mut sub, &hold);
        {
            let sent = sent.lock().unwrap();
            assert!(sent.last().unwrap().reset);
            assert_eq!(marked(&sent, 0), [46], "the hold's end re-sends it");
        }
        // REVERSE: the marks map through frames on a loop that is not whole bins. Buffer bin 46 holds
        // frames [47_104, 48_128), heard backwards at [47_872, 48_896): heard bins 46 and 47 (the bin
        // count's reversal would send bin 47 alone, which holds none of the dropout's heard frames).
        m.send(Command::Reverse(0));
        m.render(256, false, false);
        turn(&mut feed, &mut sub, &hold);
        assert!(m.view(0).reversed);
        assert_eq!(marked(&sent.lock().unwrap(), 0), [46, 47], "reversed, through frames");
    }

    #[test]
    fn a_stops_restore_and_a_new_pass_resend_the_bins_they_clear() {
        // An overdub with a jump in it marks its hole; a tick sends that; STOP discards the layer, and its
        // restore takes the marks with the audio: the next tick re-sends those bins unmarked.
        let mut m = Manual::new(4096);
        let mut feed = Feed::new(m.host.clone());
        let hold = Arc::new(Hold::default());
        let (mut sub, sent) = subscriber();
        turn(&mut feed, &mut sub, &hold);
        for c in [Command::SetBpm(240.0), Command::SetFixedLength(true), Command::SetFixedBars(2.0), Command::RecDub(0)] {
            m.send(c);
        }
        m.render(2 * 48_000 + 48_000 + 256, false, false);
        m.send(Command::SetFixedLength(false));
        m.render(4096, false, false);
        turn(&mut feed, &mut sub, &hold);
        m.send(Command::RecDub(0));
        m.render(10_000, false, false);
        m.jump(3000);
        m.render(2000, false, false);
        turn(&mut feed, &mut sub, &hold);
        let held = marked(&sent.lock().unwrap(), 0);
        assert!(!held.is_empty(), "the hole is marked on the feed");
        m.send(Command::Stop(0));
        m.render(4096, false, false);
        turn(&mut feed, &mut sub, &hold);
        {
            let sent = sent.lock().unwrap();
            let marks = sent_marks(&sent, 0);
            assert!(held.iter().all(|b| marks.get(b) == Some(&false)), "the cleared bins re-sent unmarked");
        }

        // A RETAKE roll: a dropout in pass 1, kept marked in the free buffer; pass 3 records into that
        // buffer again, cleared at its edge: none of its bins goes out marked.
        m.send(Command::ClearAll);
        m.render(4096, false, false);
        for c in [Command::SetFixedLength(true), Command::SetFixedBars(1.0), Command::SetRetake(true), Command::RecDub(1)] {
            m.send(c);
        }
        m.render(128, false, false);
        let start = m.frame + 48_000 - 128;
        m.render(start - m.frame + 10_000, false, false);
        m.render(128, false, true); // pass 1
        m.render(start + 2 * 48_000 + 1000 - m.frame, false, false); // into pass 3
        turn(&mut feed, &mut sub, &hold);
        let before = sent.lock().unwrap().len();
        for _ in 0..20 {
            m.render(2048, false, false);
            turn(&mut feed, &mut sub, &hold);
        }
        let sent = sent.lock().unwrap();
        assert_eq!(m.view(1).state, LaneState::Recording);
        let late: Vec<&PeakUpdate> = sent[before..].iter().flat_map(|f| f.peaks.iter()).filter(|p| p.lane == 1).collect();
        assert!(!late.is_empty() && late.iter().all(|p| p.marked.is_empty()), "pass 3's bins carry no mark of pass 1's");
    }

    #[test]
    fn a_multiply_over_a_buffer_that_held_marks_sends_none_of_them() {
        // Lane 0 held a 4-bar take marked in bar 4; CLEAR ALL leaves the mark in its buffer, unseen. A
        // 2-bar take there, then a multiply to 4 bars from lane 1: bars 3 and 4 tile the new loop, and no
        // stale mark goes out.
        let mut m = Manual::new(4096);
        let mut feed = Feed::new(m.host.clone());
        let hold = Arc::new(Hold::default());
        let (mut sub, sent) = subscriber();
        turn(&mut feed, &mut sub, &hold);
        for c in [Command::SetBpm(240.0), Command::SetFixedLength(true), Command::SetFixedBars(4.0), Command::RecDub(0)] {
            m.send(c);
        }
        m.render(128, false, false);
        m.render(48_000 - 128 + 3 * 48_000 + 1000, false, false);
        m.render(128, false, true);
        m.render(48_000, false, false);
        turn(&mut feed, &mut sub, &hold);
        assert!(!marked(&sent.lock().unwrap(), 0).is_empty(), "the 4-bar take is marked in bar 4");
        m.send(Command::ClearAll);
        m.render(8192, false, false);
        for c in [Command::SetFixedBars(2.0), Command::RecDub(0)] {
            m.send(c);
        }
        m.render(48_000 + 2 * 48_000 + 256, false, false);
        m.render(8192, false, false);
        for c in [Command::SetFixedBars(4.0), Command::RecDub(1)] {
            m.send(c);
        }
        let before = sent.lock().unwrap().len();
        m.render(2 * 96_000 + 96_000, false, false);
        for _ in 0..10 {
            m.render(4096, false, false);
            turn(&mut feed, &mut sub, &hold);
        }
        assert_eq!(m.view(0).frames, 192_000, "lane 0 multiplied to 4 bars");
        let sent = sent.lock().unwrap();
        assert!(sent[before..].iter().flat_map(|f| f.peaks.iter()).all(|p| p.marked.is_empty()), "no stale mark is sent");
    }

    #[test]
    fn a_held_feed_sends_nothing_keeps_its_mirror_and_ends_with_one_reset() {
        let device = engine();
        let mut feed = Feed::new(device.host().clone());
        let hold = Arc::new(Hold::default());
        let (mut sub, sent) = subscriber();
        turn(&mut feed, &mut sub, &hold);
        assert_eq!(counts(&sent), (1, 1), "a subscriber's first frame is its reset");
        // Not held: an engine event is a frame.
        set_bpm(&device, 200.0);
        turn(&mut feed, &mut sub, &hold);
        assert_eq!(sent.lock().unwrap().last().map(|f| (f.seq, f.reset, tempo(f))), Some((1, false, Some(200))));

        let guard = FeedHold::take(&hold);
        set_bpm(&device, 240.0);
        for _ in 0..500 {
            turn(&mut feed, &mut sub, &hold);
        }
        assert_eq!(sent.lock().unwrap().len(), 2, "held: no frame, however many ticks pass");
        // The held ticks took the event out of the engine's ring themselves: nothing is left for the
        // tick that ends the hold to find there.
        let mut left = Vec::new();
        device.host().drain_feed(&mut left);
        assert!(left.is_empty(), "the held ticks drained the ring: {} events were still in it", left.len());
        // A document that subscribes meanwhile waits for its reset too.
        let (mut sub, sent) = subscriber();
        for _ in 0..500 {
            turn(&mut feed, &mut sub, &hold);
        }
        assert_eq!(sent.lock().unwrap().len(), 0, "a subscriber that came while held gets nothing yet");

        drop(guard);
        turn(&mut feed, &mut sub, &hold);
        {
            let sent = sent.lock().unwrap();
            assert_eq!(sent.len(), 1, "one frame ends the hold");
            let frame = &sent[0];
            assert!(frame.reset && frame.settings.is_some(), "a reset, as a fresh subscriber gets");
            assert_eq!(frame.seq, 2, "the held ticks built no frame: the count goes on where it stopped");
            assert_eq!(tempo(frame), Some(240), "the event drained while held is in the mirror the reset carries");
        }
        set_bpm(&device, 120.0);
        for _ in 0..10 {
            turn(&mut feed, &mut sub, &hold);
        }
        let sent = sent.lock().unwrap();
        assert_eq!((sent.len(), sent.iter().filter(|f| f.reset).count()), (2, 1), "then ordinary frames again, and no second reset");
        assert_eq!((sent[1].seq, tempo(&sent[1])), (3, Some(120)));
    }

    // Nobody subscribed (before the first page, or after a page died): the feed ticks as a held one
    // does. The ring is drained into the mirror, no frame is built, and a device event waits for the
    // next subscriber's reset instead of going out to nobody.
    #[test]
    fn with_no_subscriber_the_feed_builds_nothing_and_keeps_the_device_events() {
        let device = engine();
        let mut feed = Feed::new(device.host().clone());
        let hold = Arc::new(Hold::default());
        let mut nobody = None;
        set_bpm(&device, 240.0);
        device.host().core.device_events.lock().unwrap().push(DeviceEvent::EngineFaulted);
        for _ in 0..10 {
            turn(&mut feed, &mut nobody, &hold);
        }
        let mut left = Vec::new();
        device.host().drain_feed(&mut left);
        assert!(left.is_empty(), "the ticks drained the ring: {} events were still in it", left.len());

        let (mut sub, sent) = subscriber();
        turn(&mut feed, &mut sub, &hold);
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        let frame = &sent[0];
        assert_eq!((frame.seq, frame.reset), (0, true), "no frame was built for nobody");
        assert_eq!(frame.device, [DeviceEvent::EngineFaulted], "the device event waited for a subscriber");
        assert_eq!(tempo(frame), Some(240), "the mirror followed the ring");
    }

    #[test]
    fn a_hold_released_at_once_sends_exactly_one_reset() {
        let device = engine();
        let mut feed = Feed::new(device.host().clone());
        let hold = Arc::new(Hold::default());
        let (mut sub, sent) = subscriber();
        turn(&mut feed, &mut sub, &hold);
        assert_eq!(sent.lock().unwrap().len(), 1);
        // Taken and dropped between two ticks: no tick ever saw the feed held.
        drop(FeedHold::take(&hold));
        for _ in 0..10 {
            turn(&mut feed, &mut sub, &hold);
        }
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 2, "one frame, and nothing after it while nothing changes");
        assert!(sent[1].reset && sent[1].seq == 1);
    }

    /// Two overlapping holds end when the last one drops; a guard dropped by an early return (an
    /// error on its way out with `?`) releases like any other.
    #[test]
    fn the_guard_releases_on_an_early_return_and_the_last_of_two_ends_the_hold() {
        fn fails_while_holding(hold: &Arc<Hold>) -> Result<(), String> {
            let _held = FeedHold::take(hold);
            Err("the dialog did not run".to_string())?;
            unreachable!("the error left through `?`")
        }
        let device = engine();
        let mut feed = Feed::new(device.host().clone());
        let hold = Arc::new(Hold::default());
        let (mut sub, sent) = subscriber();
        turn(&mut feed, &mut sub, &hold);

        assert!(fails_while_holding(&hold).is_err());
        assert_eq!(hold.holders.load(Acquire), 0, "the early return dropped the guard");
        turn(&mut feed, &mut sub, &hold);
        assert_eq!(counts(&sent), (2, 2), "and the feed resyncs");

        let (first, second) = (FeedHold::take(&hold), FeedHold::take(&hold));
        drop(first);
        set_bpm(&device, 240.0);
        turn(&mut feed, &mut sub, &hold);
        assert_eq!(sent.lock().unwrap().len(), 2, "still held by the second");
        drop(second);
        turn(&mut feed, &mut sub, &hold);
        assert_eq!(counts(&sent), (3, 3));
    }

    /// The scope taps on the feed (`lf_engine::scope`): nothing until the UI asks for them, then
    /// batches that splice onto each other by frame, a `gap` wherever the trace broke (the first batch,
    /// the columns a held tick dropped), and no columns on a reset frame.
    #[test]
    fn the_feed_carries_scope_columns_and_says_where_the_trace_broke() {
        // A slow device: a column (192 frames at 48 kHz) takes three blocks, so a tick's batch stays
        // far under the cap however the test machine schedules this thread.
        let device = TestDevice::start(48_000, 64, Duration::from_millis(10), |_| 0.0);
        assert!(device.wait_blocks(4, Duration::from_secs(5)), "the engine renders");
        let mut feed = Feed::new(device.host().clone());
        let hold = Arc::new(Hold::default());
        let (mut sub, sent) = subscriber();
        let batches = |sent: &Sent| sent.lock().unwrap().iter().filter_map(|f| f.scope.clone()).collect::<Vec<_>>();
        let columns = |batch: &ScopeUpdate| batch.min[0].len();
        turn(&mut feed, &mut sub, &hold);
        assert!(device.wait_blocks(12, Duration::from_secs(5)));
        turn(&mut feed, &mut sub, &hold);
        assert!(batches(&sent).is_empty(), "nobody asked for the scope");

        device.host().send(TimedCommand { frame: None, command: Command::SetScope(true) }).unwrap();
        assert!(device.wait_blocks(12, Duration::from_secs(5)));
        turn(&mut feed, &mut sub, &hold);
        let first = batches(&sent).pop().expect("the columns reach the feed");
        assert_eq!((first.bin, first.min.len(), first.max.len()), (192, SCOPE_SOURCES, SCOPE_SOURCES));
        assert!(first.min.iter().chain(&first.max).all(|c| c.len() == columns(&first)), "every array holds the same columns");
        assert!(first.gap, "the first batch has nothing to splice onto");

        assert!(device.wait_blocks(9, Duration::from_secs(5)));
        turn(&mut feed, &mut sub, &hold);
        let next = batches(&sent).pop().expect("a second batch");
        assert!(!next.gap, "it splices onto the one before it");
        assert_eq!(next.frame, first.frame + columns(&first) as Frame * first.bin as Frame, "no hole and no overlap between batches");

        // A hole inside one drain (the taps switched off and on between two ticks): only the newest run
        // goes out, and it says the trace broke. Were the whole drain sent, its `frame` would be where
        // the batch above ended and its `bin` would claim the hole never happened.
        let spliced = next.frame + columns(&next) as Frame * next.bin as Frame;
        assert!(device.wait_blocks(12, Duration::from_secs(5)), "columns before the switch");
        device.host().send(TimedCommand { frame: None, command: Command::SetScope(false) }).unwrap();
        assert!(device.wait_blocks(30, Duration::from_secs(5)), "the hole: frames no column covers");
        device.host().send(TimedCommand { frame: None, command: Command::SetScope(true) }).unwrap();
        assert!(device.wait_blocks(12, Duration::from_secs(5)), "columns after it");
        turn(&mut feed, &mut sub, &hold);
        let run = batches(&sent).pop().expect("a batch after the hole");
        assert!(run.gap, "a batch that cannot be spliced says so");
        assert!(run.frame > spliced, "it is the run after the hole, not the one before it ({} against {spliced})", run.frame);
        assert!(columns(&run) < 6, "the run before the hole is not in it ({} columns)", columns(&run));

        // Held, the columns are drained and dropped: the hold's reset carries none, and the batch after
        // it says the trace broke.
        let guard = FeedHold::take(&hold);
        assert!(device.wait_blocks(12, Duration::from_secs(5)));
        for _ in 0..4 {
            turn(&mut feed, &mut sub, &hold);
        }
        let sent_while_held = batches(&sent).len();
        drop(guard);
        // Columns waiting when the hold ends: the resync frame still carries none.
        assert!(device.wait_blocks(9, Duration::from_secs(5)));
        turn(&mut feed, &mut sub, &hold);
        {
            let frames = sent.lock().unwrap();
            let resync = frames.iter().rposition(|f| f.reset).expect("the hold ends with a reset");
            assert!(frames[resync].scope.is_none(), "a reset frame carries no columns");
        }
        assert!(device.wait_blocks(9, Duration::from_secs(5)));
        turn(&mut feed, &mut sub, &hold);
        let after = batches(&sent);
        assert_eq!(after.len(), sent_while_held + 1, "the held ticks sent none, and one batch came after");
        assert!(after[after.len() - 1].gap, "the columns the hold dropped broke the trace");
    }

    /// The thread itself: held, it sends nothing while the engine reports; released, its next frame
    /// is a reset; and `stop` ends it while it is held.
    #[test]
    fn the_feed_thread_holds_resyncs_and_still_stops_while_held() {
        let wait_for = |pred: &dyn Fn() -> bool| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !pred() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(2));
            }
            pred()
        };
        let device = engine();
        let thread = FeedThread::spawn(device.host().clone()).unwrap();
        let sent = Sent::default();
        let sink = sent.clone();
        thread.subscribe(move |frame| {
            sink.lock().unwrap().push(frame);
            true
        });
        assert!(wait_for(&|| sent.lock().unwrap().len() == 1), "the subscriber's reset");

        let held = thread.hold();
        set_bpm(&device, 240.0);
        std::thread::sleep(TICK * 12);
        assert_eq!(sent.lock().unwrap().len(), 1, "held: nothing is sent");
        drop(held);
        assert!(wait_for(&|| sent.lock().unwrap().len() == 2), "the hold's end sends a frame");
        {
            let sent = sent.lock().unwrap();
            assert!(sent[1].reset && sent[1].seq == 1 && tempo(&sent[1]) == Some(240));
        }

        let _held = thread.hold();
        let (done, stopped) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                thread.stop();
                let _ = done.send(());
            });
            assert!(stopped.recv_timeout(Duration::from_secs(5)).is_ok(), "a held feed thread still stops");
        });
    }
}
