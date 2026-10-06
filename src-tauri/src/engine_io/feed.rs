//! OWNS: the feed (its frame: `wire.rs`): what the UI reads back from the
//! engine, built on its own non-RT thread about 60 times a second ([`FeedFrame`]). Each tick drains the
//! engine's events and the device's, and reads the device status, the clock anchor, the input meter
//! and the lanes' waveforms; a frame goes out when any of them changed, and at least every `REFRESH`
//! while a device runs (the UI extrapolates its playhead from the anchor between frames). Never PCM.
//!
//! The feed is the only reader of the engine's event ring and of the device events, but for a rebuild,
//! which drains the replaced engine's ring into the settings memory (`owner.rs` `swap_engine`). So it
//! keeps a mirror of the lanes, the transport and the selection: a new subscriber (a WebView reload) and
//! a new engine (another sample rate, a fault) get a `reset` frame that carries all of it, with the
//! settings the host keeps and each lane's last `Mix` (`settings.rs`, which every drain feeds), so the
//! UI adopts them. The thread drains while nobody subscribes, so the event ring never fills.
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
use lf_engine::{Event, LaneInfo, LaneState, Overview, TRACK_COUNT};

use super::wire::{ClockAnchor, FeedFrame, Meter, PeakUpdate, WireCommand, WireEvent};
use super::{DeviceEvent, DeviceStatus, EngineHost};

/// The device events a held feed keeps for its reset frame; older ones go first.
const HELD_DEVICE_EVENTS: usize = 64;

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
        }
    }

    /// Drain the engine's events into the mirror. True when another engine replaced the one the
    /// mirror followed: the UI must reset.
    fn drain(&mut self) -> bool {
        let mut replaced = false;
        self.drained.clear();
        let (gen, overview) = self.host.drain_feed(&mut self.drained);
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

    /// A tick of a held feed: the ring is drained and the mirror follows it, the device events are
    /// kept for the next frame (the latest `HELD_DEVICE_EVENTS`), and no frame is built (`seq` stays). The reset that ends the hold
    /// reads the status, the meter and the waveforms afresh.
    pub(crate) fn tick_held(&mut self) {
        self.drain();
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
        let due = running && self.sent.is_none_or(|at| at.elapsed() >= REFRESH);
        if !(reset || due || metered || status.is_some() || !self.drained.is_empty() || !device.is_empty() || !peaks.is_empty()) {
            return None;
        }
        let events = if reset { self.state_events() } else { self.drained.iter().copied().map(WireEvent).collect() };
        let settings = reset.then(|| self.host.settings().into_iter().map(WireCommand).collect());
        self.sent = Some(Instant::now());
        self.seq += 1;
        Some(FeedFrame { seq: self.seq - 1, reset, events, device, status, anchor, meter, peaks, settings })
    }

    /// The lanes' waveform bins that changed, in play order: every bin of a lane that shows another
    /// buffer or orientation, holds less than before, grows while reversed (a multiply: its play order
    /// counts back from the new end) or is new to the UI (a reset), else the bins its buffer changed plus
    /// the ones it grew by.
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

/// One tick of the feed thread, under the subscriber's lock. Held: the feed ticks and nothing is
/// sent, to a subscriber that came meanwhile neither (its reset waits for the hold's end). Else one
/// frame at most goes out, a reset when the subscriber is new or a hold has ended since the last tick.
fn turn(feed: &mut Feed, subscriber: &mut Option<Subscriber>, hold: &Hold) {
    if hold.holders.load(Acquire) > 0 {
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
    use lf_engine::{Command, TimedCommand};

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
