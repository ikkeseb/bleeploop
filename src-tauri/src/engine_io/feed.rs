//! OWNS: the feed (its frame: `wire.rs`): what the UI reads back from the
//! engine, built on its own non-RT thread about 60 times a second ([`FeedFrame`]). Each tick drains the
//! engine's events and the device's, and reads the device status, the clock anchor, the input meter,
//! each plugin slot's live flag and the lanes' waveforms; a frame goes out when any of them changed, and at least every `REFRESH`
//! while a device runs (the UI extrapolates its playhead from the anchor between frames). Never PCM.
//!
//! The scope taps (`lf_engine::scope`) are drained every tick whatever the frame carries, held or not:
//! their ring left to fill would make the next batch a replay of seconds of old sound as a fast light
//! show. A batch the UI cannot splice onto what it holds (columns the engine or a held tick lost, the
//! cap, a new engine, a reset frame) carries `gap`, and the UI drops its trace instead.
//!
//! The feed is the only reader of the engine's event ring and of the device events, but for a rebuild,
//! which drains the replaced engine's ring into the settings memory (`owner.rs` `swap_engine`). So it
//! keeps a mirror of the lanes, the transport, the selection and the current count-in: a new subscriber
//! (a WebView reload) and a new engine (another sample rate, a fault) get a `reset` frame carrying it, with the
//! settings the host keeps and each lane's last `Mix` (`settings.rs`, which every drain feeds), so the
//! UI adopts them. The thread drains while nobody subscribes, so the event ring never fills; it then
//! ticks as a held feed does (below): no frame is built, and the device events wait for the next
//! subscriber's reset.
//!
//! `Channel::send` succeeds once Tauri has queued a frame, not once the page read it, so the page acks
//! (`engine_feed_ack`, from `src/platform/host.tauri.ts`): the newest `seq` its channel handed on, 100 ms
//! after a frame and every second, `null` before its first. Acks are cumulative and fenced by the
//! subscriber's own range (`seq` rises across subscribers), so an older channel's ack covers nothing.
//! `MAX_IN_FLIGHT` frames unacked WITHHOLD the subscriber until every frame sent is acked: a stalled
//! or slow page lags at most that many frames, never without bound.
//!
//! The feed can also be HELD (`FeedThread::hold`) while the UI thread cannot read (the native folder
//! dialog is modal on it). Held, withheld or unsubscribed, a tick drains the ring into the mirror and
//! keeps the drained events in order, beats excepted (`WITHHELD_EVENTS`; `Lane`, `Mix` and
//! `Transport` publish only on change, so the stream grows with the player's actions and RETAKE's
//! passes, not with time), builds and sends nothing, leaves the waveform dirty bits alone and keeps
//! device events (`HELD_DEVICE_EVENTS`). `seq` counts frames built, so it does not move meanwhile. The first frame
//! after is a CATCH-UP (`catch_up`, not a reset): the kept stream, the count's latest beat and downbeat
//! while a lane is armed, then this tick's events; the peaks changed since the last frame sent; scope
//! with `gap`. The page applies it as it would have live (`applyFrameNow`,
//! `src/ui/state/engine-store.ts`), so overlays, revisions and the clear token survive. A RESET instead
//! when the subscriber is new, another engine came meanwhile (latched by the held tick) or the stream
//! passed `WITHHELD_EVENTS`; every reset carries the notices kept meanwhile (`TakeRejected`,
//! `PassDropped`, `Refused`, the newest `KEPT_NOTICES`), so a notice outlives another engine or an
//! overflow. Late is still late: a catch-up's events read the page's state as it is when they arrive.
//!
//! A frame lost before the page's handler (a failed payload fetch, which Tauri only logs) parks every
//! later one. The watchdog judges ack progress, not a frame's age: acks that do not advance while frames
//! are in flight answer `true` after `STUCK`, and the page retires that channel (its late frames and
//! answers fenced by a generation) and subscribes again. A new subscriber takes over the notices of
//! the last one's unacked frames, and its reset carries them.

use std::collections::VecDeque;
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
use lf_engine::{Event, LaneInfo, LaneState, Overview, SLOT_COUNT, TRACK_COUNT};

use super::wire::{ClockAnchor, FeedFrame, Meter, PeakUpdate, ScopeUpdate, WireCommand, WireEvent};
use super::{DeviceEvent, DeviceStatus, EngineHost};

/// The device events an unsent feed keeps for its next frame; older ones go first.
const HELD_DEVICE_EVENTS: usize = 64;
/// The frames a subscriber may leave unacked before it is withheld: about half a second of them.
const MAX_IN_FLIGHT: usize = 30;
/// The events an unsent stream keeps for its catch-up; past it the next frame resets instead.
const WITHHELD_EVENTS: usize = 256;
/// The notices a reset carries from the frames the page missed; older ones go first.
const KEPT_NOTICES: usize = 32;
/// How long acks may stand still while frames are in flight before the page is told to resubscribe.
const STUCK: Duration = Duration::from_secs(5);

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
    clear_armed: false,
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
    /// The count's latest positive beat and its first downbeat, until no lane is armed (a count's
    /// first beat can drain before its lane's arm); a lane leaving armed ends the count it held,
    /// dropping a count beat older than that lane event, and that beat's downbeat with it.
    count_beat: Option<Event>,
    count_downbeat: Option<Event>,
    /// What the last frames said.
    status: Option<DeviceStatus>,
    meter: Option<Meter>,
    /// Each plugin slot's live flag as last sent (`Overview::slot_live`; all off with no engine).
    live: [bool; SLOT_COUNT],
    sent: Option<Instant>,
    drained: Vec<Event>,
    /// A hold, a full subscriber or nobody subscribed missed ticks: the next frame ends the stream.
    withheld: bool,
    withheld_events: Vec<Event>,
    overflowed: bool,
    /// Another engine arrived during an unsent tick: the next frame must reset.
    replaced: bool,
    /// Notices survive a reset even when its engine or the bounded stream no longer does.
    kept_notices: Vec<Event>,
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
            count_beat: None,
            count_downbeat: None,
            status: None,
            meter: None,
            live: [false; SLOT_COUNT],
            sent: None,
            drained: Vec::with_capacity(256),
            withheld: false,
            withheld_events: Vec::new(),
            overflowed: false,
            replaced: false,
            kept_notices: Vec::new(),
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
        self.drained.clear();
        self.columns.clear();
        // The events and the columns in one acquisition, so this tick's columns are the engine whose
        // events it took: a second drain could straddle a rebuild and mix the two.
        let (gen, overview, bin) = self.host.drain_feed_scope(&mut self.drained, Some(&mut self.columns));
        self.scope_bin = bin;
        if self.gen != Some(gen) {
            self.overview = overview;
        }
        self.update_mirror(gen)
    }

    /// Apply one drain without reading a device. A generation change drops the old engine's mirror.
    fn update_mirror(&mut self, gen: u64) -> bool {
        let mut replaced = false;
        if self.gen != Some(gen) {
            // A new engine (or none): what it reports starts from EMPTY lanes and no transport.
            replaced = self.gen.is_some();
            self.gen = Some(gen);
            self.lanes = [None; TRACK_COUNT];
            self.transport = None;
            self.selected = (0, 0);
            self.count_beat = None;
            self.count_downbeat = None;
        }
        // A count beat no disarm followed in this drain: its lane's arm may be the next push, which this
        // drain did not reach (the ring is read while the callback writes it).
        let mut count_unended = false;
        for event in &self.drained {
            match *event {
                Event::Lane { frame, lane, info } if usize::from(lane) < TRACK_COUNT => {
                    let lane = usize::from(lane);
                    if self.lanes[lane].is_some_and(|(_, previous)| previous.armed) && !info.armed {
                        count_unended = false;
                        // Beats precede the lane publish: a count beat at this frame belongs to a new count.
                        // The old count's downbeat goes with its beat, though it fell on this frame.
                        if matches!(self.count_beat, Some(Event::Beat { frame: beat_frame, .. }) if beat_frame < frame) {
                            self.count_beat = None;
                            self.count_downbeat = None;
                        }
                    }
                    self.lanes[lane] = Some((frame, info));
                }
                Event::Transport { .. } => self.transport = Some(*event),
                Event::Selected { frame, lane } => self.selected = (frame, lane),
                Event::Beat { count_left, .. } if count_left > 0 => {
                    self.count_beat = Some(*event);
                    self.count_downbeat = None;
                    count_unended = true;
                }
                Event::Beat { count_left: 0, .. } if self.count_beat.is_some() && self.count_downbeat.is_none() => self.count_downbeat = Some(*event),
                _ => {}
            }
        }
        // A count beat precedes its lane's arm, in this drain or the next: judge after every lane was applied.
        if !count_unended && !self.lanes.iter().flatten().any(|(_, info)| info.armed) {
            self.count_beat = None;
            self.count_downbeat = None;
        }
        replaced
    }

    /// Keep an unsent drain in order, without beats. Past the bound only the notices survive: the
    /// next frame resets from the mirror instead of replaying an incomplete stream.
    fn keep_drained(&mut self) {
        self.withheld = true;
        for &event in &self.drained {
            if is_notice(&event) {
                self.kept_notices.push(event);
            }
            if !matches!(event, Event::Beat { .. }) && !self.overflowed {
                if self.withheld_events.len() == WITHHELD_EVENTS {
                    self.overflowed = true;
                    self.withheld_events.clear();
                } else {
                    self.withheld_events.push(event);
                }
            }
        }
        keep_newest(&mut self.kept_notices);
    }

    /// A held, withheld or unsubscribed tick drains the ring into the mirror and keeps the events
    /// for its next frame. It leaves waveform dirty bits and last-sent status, live and meter alone.
    pub(crate) fn tick_held(&mut self) {
        self.host.publish_if_idle();
        let replaced = self.drain();
        self.replaced |= replaced;
        self.keep_drained();
        self.scope(false);
        self.held_device.extend(self.host.take_device_events());
        // A device that keeps failing behind a dialog left open: the latest are what the page needs.
        let over = self.held_device.len().saturating_sub(HELD_DEVICE_EVENTS);
        self.held_device.drain(..over);
    }

    /// Read everything once; the frame to send, if anything changed or the anchor is due. `reset`: a
    /// new subscriber: the frame carries the whole state. An unsent stream ends with a catch-up,
    /// unless another engine or an overflow requires a reset.
    pub(crate) fn tick(&mut self, mut reset: bool) -> Option<FeedFrame> {
        // A stopped engine renders no block that would offer again what its full ring refused.
        self.host.publish_if_idle();
        reset |= self.drain() || self.replaced || self.overflowed;
        let catch_up = self.withheld && !reset;
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
        let now_live: [bool; SLOT_COUNT] = std::array::from_fn(|s| self.overview.as_ref().is_some_and(|o| o.slot_live(s)));
        let live = (reset || now_live != self.live).then_some(now_live);
        self.live = now_live;
        let peaks = self.peaks(reset);
        // A reset carries no scope: the UI replaces its view with this frame, so the trace starts over.
        self.scope_gap |= catch_up;
        let scope = self.scope(!reset);
        let due = running && self.sent.is_none_or(|at| at.elapsed() >= REFRESH);
        // A batch of columns is itself a reason to send: a tick that built one never drops it.
        if !(reset || catch_up || due || metered || live.is_some() || status.is_some() || !self.drained.is_empty() || !device.is_empty() || !peaks.is_empty() || scope.is_some()) {
            return None;
        }
        let events = if reset {
            self.state_events()
        } else if catch_up {
            self.withheld_events.iter().copied().chain(self.count_events()).chain(self.drained.iter().copied()).map(WireEvent).collect()
        } else {
            self.drained.iter().copied().map(WireEvent).collect()
        };
        let settings = reset.then(|| self.host.settings().into_iter().map(WireCommand).collect());
        self.withheld = false;
        self.withheld_events.clear();
        self.overflowed = false;
        self.replaced = false;
        self.kept_notices.clear();
        self.sent = Some(Instant::now());
        self.seq += 1;
        Some(FeedFrame { seq: self.seq - 1, reset, catch_up, events, device, status, anchor, meter, live, peaks, scope, settings })
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
    /// `Mix` the host kept (a lane no engine has reported has none), the selected lane, the current count's
    /// latest beat and downbeat, the kept notices, then this tick's other one-shots.
    fn state_events(&self) -> Vec<WireEvent> {
        let lanes = self.lanes.iter().enumerate().map(|(i, lane)| {
            let (frame, info) = lane.unwrap_or((0, EMPTY_LANE));
            Event::Lane { frame, lane: i as u8, info }
        });
        let selected = Event::Selected { frame: self.selected.0, lane: self.selected.1 };
        // Beats drained this tick are already in `rest`. Replaying a past count beat leaves the beat
        // LED on its beat_in_bar until the next live beat; that is acceptable on a reset.
        let rest = self.drained.iter().copied().filter(|e| !matches!(e, Event::Lane { .. } | Event::Transport { .. } | Event::Selected { .. } | Event::Mix { .. }));
        self.transport.into_iter().chain(lanes).chain(self.host.mixes()).chain([selected]).chain(self.count_events()).chain(self.kept_notices.iter().copied()).chain(rest).map(WireEvent).collect()
    }

    /// The mirror's count, except beats this tick already carries.
    fn count_events(&self) -> impl Iterator<Item = Event> + '_ {
        self.count_beat.into_iter().chain(self.count_downbeat).filter(move |e| !self.drained.contains(e))
    }
}

fn is_notice(event: &Event) -> bool {
    matches!(event, Event::TakeRejected { .. } | Event::PassDropped { .. } | Event::Refused { .. })
}

fn keep_newest(notices: &mut Vec<Event>) {
    let over = notices.len().saturating_sub(KEPT_NOTICES);
    notices.drain(..over);
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
    in_flight: VecDeque<InFlight>,
    first: Option<u64>,
    last: Option<u64>,
    newest_ack: Option<u64>,
    withheld: bool,
    behind_since: Option<Instant>,
    /// An old channel's unacked notices, waiting for this subscriber's first reset.
    notices: Vec<Event>,
}

struct InFlight {
    seq: u64,
    sent: Instant,
    notices: Vec<Event>,
}

impl Subscriber {
    fn new(send: Sink, previous: Option<Subscriber>) -> Subscriber {
        let notices = previous.map_or_else(Vec::new, Subscriber::unacked_notices);
        Subscriber {
            send, fresh: true, in_flight: VecDeque::new(), first: None, last: None,
            newest_ack: None, withheld: false, behind_since: None, notices,
        }
    }

    fn unacked_notices(self) -> Vec<Event> {
        let mut notices = self.notices;
        notices.extend(self.in_flight.into_iter().flat_map(|f| f.notices));
        keep_newest(&mut notices);
        notices
    }

    fn record(&mut self, frame: &FeedFrame, now: Instant) {
        self.first.get_or_insert(frame.seq);
        self.last = Some(frame.seq);
        let notices = frame.events.iter().map(|e| e.0).filter(is_notice).collect();
        self.in_flight.push_back(InFlight { seq: frame.seq, sent: now, notices });
    }

    /// Cumulative ack, fenced by this subscriber's frame range. Only progress restarts the watchdog;
    /// a first heartbeat after a long page stall starts it now, never at the oldest frame's age.
    fn ack(&mut self, seq: Option<u64>, now: Instant) -> bool {
        if let Some(seq) = seq.filter(|&seq| self.first.is_some_and(|first| seq >= first) && self.last.is_some_and(|last| seq <= last)) {
            if self.newest_ack.is_none_or(|ack| seq > ack) {
                self.newest_ack = Some(seq);
                while self.in_flight.front().is_some_and(|f| f.seq <= seq) {
                    self.in_flight.pop_front();
                }
                self.behind_since = None;
                return false;
            }
        }
        let Some(first) = self.in_flight.front() else {
            self.behind_since = None;
            return false;
        };
        let since = *self.behind_since.get_or_insert(now.max(first.sent));
        now.saturating_duration_since(since) >= STUCK
    }
}

/// What holds the feed's sends: how many guards are out, and that one was dropped since the thread
/// last sent (the UI missed frames, or could have: it gets a catch-up).
#[derive(Default)]
struct Hold {
    holders: AtomicUsize,
    resync: AtomicBool,
}

/// The feed sends nothing while one of these lives (`FeedThread::hold`). Its drop ends the hold on
/// every path out of its scope, and the feed's next unblocked tick sends one catch-up frame.
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
/// reset waits for the hold's end). A full subscriber stays withheld until every frame is acked.
/// Else one frame at most goes out, a new subscriber's reset taking precedence over a catch-up.
fn turn(feed: &mut Feed, subscriber: &mut Option<Subscriber>, hold: &Hold) {
    if let Some(s) = subscriber.as_mut() {
        if s.in_flight.len() >= MAX_IN_FLIGHT {
            s.withheld = true;
        } else if s.in_flight.is_empty() {
            s.withheld = false;
        }
    }
    if hold.holders.load(Acquire) > 0 || subscriber.as_ref().is_none_or(|s| s.withheld) {
        feed.tick_held();
        return;
    }
    feed.withheld |= hold.resync.swap(false, AcqRel);
    let fresh = subscriber.as_mut().is_some_and(|s| std::mem::take(&mut s.fresh));
    if let Some(s) = subscriber.as_mut() {
        let mut notices = std::mem::take(&mut s.notices);
        notices.append(&mut feed.kept_notices);
        keep_newest(&mut notices);
        feed.kept_notices = notices;
    }
    if let (Some(frame), Some(s)) = (feed.tick(fresh), subscriber.as_mut()) {
        s.record(&frame, Instant::now());
        if !(s.send)(frame) {
            log::warn!("[engine_io] the feed's subscriber is gone");
            feed.kept_notices = subscriber.take().unwrap().unacked_notices();
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
        let mut sub = self.subscriber.lock().unwrap_or_else(|e| e.into_inner());
        *sub = Some(Subscriber::new(Box::new(send), sub.take()));
    }

    pub(crate) fn ack(&self, seq: Option<u64>, now: Instant) -> bool {
        self.subscriber.lock().unwrap_or_else(|e| e.into_inner()).as_mut().is_some_and(|s| s.ack(seq, now))
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
    use crate::engine_io::test_rig::{TestDevice, PATIENCE};
    use lf_engine::{Command, TimedCommand};

    type Sent = Arc<Mutex<Vec<FeedFrame>>>;

    /// An engine a plain thread renders (no device, so the feed has no status, anchor or meter to
    /// send: a frame goes out only for a reset or an engine event), once it has reported its start.
    fn engine() -> TestDevice {
        let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
        assert!(device.wait_blocks(4), "the engine renders");
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
        (Some(Subscriber::new(send, None)), sent)
    }

    /// Change the tempo and wait until the engine has run the command: its transport event is in the ring.
    fn set_bpm(device: &TestDevice, bpm: f64) {
        device.host().send(TimedCommand { frame: None, command: Command::SetBpm(bpm) }).unwrap();
        assert!(device.wait_blocks(4), "the engine renders");
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

    fn beat(frame: Frame, count_left: u8) -> Event {
        Event::Beat { frame, beat_in_bar: (4 - count_left) % 4, count_left, clicked: count_left > 0 }
    }

    fn count_lane(frame: Frame, lane: u8, state: LaneState, armed: bool) -> Event {
        Event::Lane { frame, lane, info: LaneInfo { state, armed, ..EMPTY_LANE } }
    }

    /// A scripted drain on the real mirror, without an engine, device or rendering thread.
    fn mirror_tick(feed: &mut Feed, events: &[Event]) {
        feed.drained.clear();
        feed.drained.extend_from_slice(events);
        assert!(!feed.update_mirror(0));
    }

    /// A real engine and its feed, rendered only when the test asks. No device or thread.
    fn manual_engine() -> (lf_engine::Engine, Feed) {
        let core = Arc::new(super::super::Core::new());
        let config = lf_engine::EngineConfig { max_loop_seconds: 2.0, ..lf_engine::EngineConfig::new(8_000) };
        let (engine, handle) = lf_engine::Engine::new(config);
        *core.ends.lock().unwrap() = Some(super::super::Ends {
            commands: handle.commands, pushed: 0, events: handle.events, overview: handle.overview,
            session: Some(handle.session), scope: handle.scope, scope_bin: 32,
        });
        (engine, Feed::new(EngineHost { core }))
    }

    /// Render `blocks` blocks of 128 frames from `at`, as a device would, its input a constant.
    fn render(engine: &mut lf_engine::Engine, at: &mut Frame, blocks: usize) {
        for _ in 0..blocks {
            let ctx = lf_engine::ProcessContext { frame: *at, xrun: false, damaged: false, align_frames: 0, input_frames: 0 };
            engine.process(&ctx, &[0.25; 128], &mut [0.0; 128], &mut [0.0; 128]);
            *at += 128;
        }
    }

    /// Queue commands for the engine's next block.
    fn send(feed: &Feed, commands: impl IntoIterator<Item = Command>) {
        feed.host.send_all(commands.into_iter().map(|command| TimedCommand { frame: None, command })).unwrap();
    }

    /// A page reload now: a new subscriber's first frame, its reset, as events.
    fn reload(feed: &mut Feed) -> Vec<Event> {
        let (mut sub, sent) = subscriber();
        turn(feed, &mut sub, &Hold::default());
        let frames = sent.lock().unwrap();
        assert!(frames[0].reset);
        frames[0].events.iter().map(|e| e.0).collect()
    }

    /// The beats a reset replays, as (frame, count_left), and the lanes it shows armed.
    fn replayed_count(events: &[Event]) -> (Vec<(Frame, u8)>, Vec<u8>) {
        let beats = events.iter().filter_map(|e| match *e {
            Event::Beat { frame, count_left, .. } => Some((frame, count_left)),
            _ => None,
        });
        let armed = events.iter().filter_map(|e| match *e {
            Event::Lane { lane, info, .. } if info.armed => Some(lane),
            _ => None,
        });
        (beats.collect(), armed.collect())
    }

    /// A feed tick that drains while the device callback sits between two of its pushes (the ring is
    /// read without the engine lock): only the events up to the first count beat, into the mirror.
    fn drain_through_count_beat(feed: &mut Feed) -> (Frame, u8) {
        let gen = feed.host.core.engine_gen.load(Acquire);
        feed.drained.clear();
        let mut ends = feed.host.core.ends.lock().unwrap();
        let events = &mut ends.as_mut().unwrap().events;
        let beat = loop {
            let event = events.pop().expect("the block pushed a count beat");
            feed.drained.push(event);
            if let Event::Beat { frame, count_left, .. } = event {
                if count_left > 0 {
                    break (frame, count_left);
                }
            }
        };
        drop(ends);
        feed.update_mirror(gen);
        beat
    }

    /// A committed take on lane 0, stopped: the transport idle over a master, the commit's jobs done.
    fn idle_master(engine: &mut lf_engine::Engine, feed: &mut Feed, page: &mut Option<Subscriber>, at: &mut Frame) {
        send(feed, [Command::RecDub(0)]);
        render(engine, at, 100);
        send(feed, [Command::PlayStop(0)]);
        render(engine, at, 200);
        turn(feed, page, &Hold::default());
        assert_eq!(feed.lanes[0].unwrap().1.state, LaneState::Stopped);
    }

    /// Script the event ring itself: `turn` still drains through the host, mirrors and builds frames.
    fn scripted_feed() -> (Feed, rtrb::Producer<Event>) {
        let (_, feed) = manual_engine();
        let (events, reader) = rtrb::RingBuffer::new(512);
        feed.host.core.ends.lock().unwrap().as_mut().unwrap().events = reader;
        (feed, events)
    }

    fn fill_window(feed: &mut Feed, sub: &mut Option<Subscriber>, hold: &Hold, events: &mut rtrb::Producer<Event>) {
        for n in 0..MAX_IN_FLIGHT {
            events.push(Event::Selected { frame: n as Frame, lane: (n % TRACK_COUNT) as u8 }).unwrap();
            turn(feed, sub, hold);
        }
        assert_eq!(sub.as_ref().unwrap().in_flight.len(), MAX_IN_FLIGHT);
    }

    fn rejected(frame: Frame) -> Event {
        Event::TakeRejected { frame, lane: 0, overdub: true }
    }

    #[test]
    fn unacked_frames_reach_the_bound_but_the_ring_still_drains_into_the_mirror() {
        let (mut feed, mut events) = scripted_feed();
        let hold = Hold::default();
        let (mut sub, sent) = subscriber();
        fill_window(&mut feed, &mut sub, &hold, &mut events);
        let lane = count_lane(100, 0, LaneState::Recording, false);
        events.push(lane).unwrap();
        for _ in 0..100 {
            turn(&mut feed, &mut sub, &hold);
        }
        assert_eq!(counts(&sent), (MAX_IN_FLIGHT, 1));
        assert_eq!(feed.seq, MAX_IN_FLIGHT as u64, "withheld ticks build no frame");
        assert_eq!(feed.lanes[0].unwrap().1.state, LaneState::Recording);
        assert_eq!(feed.withheld_events, [lane]);
        let mut left = Vec::new();
        feed.host.drain_feed(&mut left);
        assert!(left.is_empty(), "the withheld ticks drained the ring");

        let s = sub.as_mut().unwrap();
        assert!(!s.ack(Some(MAX_IN_FLIGHT as u64 - 2), Instant::now()));
        turn(&mut feed, &mut sub, &hold);
        assert_eq!(sent.lock().unwrap().len(), MAX_IN_FLIGHT, "one unacked frame still withholds");
        assert!(!sub.as_mut().unwrap().ack(Some(MAX_IN_FLIGHT as u64 - 1), Instant::now()));
        turn(&mut feed, &mut sub, &hold);
        turn(&mut feed, &mut sub, &hold);
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), MAX_IN_FLIGHT + 1, "exactly one catch-up ends the withhold");
        assert!(sent.last().unwrap().catch_up && !sent.last().unwrap().reset);
    }

    #[test]
    fn a_catch_up_keeps_commits_clears_and_overdubs_in_order_and_the_count_once() {
        let (mut feed, mut events) = scripted_feed();
        let hold = Hold::default();
        let (mut sub, sent) = subscriber();
        fill_window(&mut feed, &mut sub, &hold, &mut events);
        let stream = [
            count_lane(100, 0, LaneState::Recording, false),
            count_lane(200, 0, LaneState::Playing, false),
            Event::Cleared { frame: 300, lane: 0 },
            count_lane(300, 0, LaneState::Empty, false),
            count_lane(400, 1, LaneState::Overdubbing, false),
            count_lane(500, 1, LaneState::Playing, false),
            count_lane(600, 1, LaneState::Overdubbing, false),
            rejected(650),
            count_lane(700, 4, LaneState::Recording, true),
        ];
        for event in stream {
            events.push(event).unwrap();
            events.push(beat(700, 4)).unwrap();
            turn(&mut feed, &mut sub, &hold);
        }
        events.push(beat(800, 1)).unwrap();
        events.push(beat(900, 0)).unwrap();
        turn(&mut feed, &mut sub, &hold);
        let current = Event::Selected { frame: 1000, lane: 1 };
        events.push(current).unwrap();
        sub.as_mut().unwrap().ack(Some(MAX_IN_FLIGHT as u64 - 1), Instant::now());
        turn(&mut feed, &mut sub, &hold);
        let frames = sent.lock().unwrap();
        let catch_up = frames.last().unwrap();
        let expected: Vec<_> = stream.into_iter().chain([beat(800, 1), beat(900, 0), current]).collect();
        assert_eq!(catch_up.events.iter().map(|e| e.0).collect::<Vec<_>>(), expected);
        assert!(catch_up.catch_up && !catch_up.reset && catch_up.settings.is_none());
        assert!(catch_up.status.is_none() && catch_up.live.is_none() && catch_up.peaks.is_empty(), "unchanged values stay absent");
        drop(frames);
        turn(&mut feed, &mut sub, &hold);
        assert_eq!(sent.lock().unwrap().len(), MAX_IN_FLIGHT + 1);
    }

    #[test]
    fn a_catch_up_carries_only_changed_peak_bins() {
        let (mut engine, mut feed) = manual_engine();
        let hold = Arc::new(Hold::default());
        let (mut sub, sent) = subscriber();
        send(&feed, [Command::SetBpm(240.0), Command::SetSlotLive(0, true), Command::RecDub(0)]);
        let mut at = 0;
        // The count is 8000 frames; leave several bins drawn while the take still records.
        render(&mut engine, &mut at, 100);
        turn(&mut feed, &mut sub, &hold);
        let overview = feed.overview.clone().unwrap();
        let before = overview.lane(0);
        assert_eq!(before.state, LaneState::Recording);
        assert!(before.frames > (3 * PEAK_FRAMES) as Frame);
        let first = sent.lock().unwrap()[0].peaks.clone();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].start, 0);
        // Fill the window without rendering or consuming any new waveform writes.
        for _ in 1..MAX_IN_FLIGHT {
            drop(FeedHold::take(&hold));
            turn(&mut feed, &mut sub, &hold);
        }
        render(&mut engine, &mut at, 4);
        turn(&mut feed, &mut sub, &hold);
        render(&mut engine, &mut at, 4);
        turn(&mut feed, &mut sub, &hold);
        assert_eq!(sent.lock().unwrap().len(), MAX_IN_FLIGHT);
        sub.as_mut().unwrap().ack(Some(MAX_IN_FLIGHT as u64 - 1), Instant::now());
        turn(&mut feed, &mut sub, &hold);
        let frames = sent.lock().unwrap();
        let catch_up = frames.last().unwrap();
        assert!(catch_up.catch_up && !catch_up.reset);
        assert_eq!(catch_up.peaks.len(), 1);
        let p = &catch_up.peaks[0];
        let after = overview.lane(0);
        assert_eq!((p.lane, p.start, p.count), (0, before.frames as u32 / PEAK_FRAMES as u32, (after.frames as usize).div_ceil(PEAK_FRAMES) as u32));
        assert_eq!(p.min.len(), (p.count - p.start) as usize, "only the bins written since the last send");
        for (offset, (&min, &max)) in p.min.iter().zip(&p.max).enumerate() {
            let (lo, hi) = overview.bin(after.buf, p.start as usize + offset);
            assert_eq!((min, max), (round(lo), round(hi)));
        }
    }

    #[test]
    fn count_beats_drained_in_the_catch_up_tick_are_sent_once() {
        let (mut feed, mut events) = scripted_feed();
        let hold = Arc::new(Hold::default());
        let (mut sub, sent) = subscriber();
        turn(&mut feed, &mut sub, &hold);
        let guard = FeedHold::take(&hold);
        let armed = count_lane(100, 4, LaneState::Recording, true);
        events.push(beat(100, 4)).unwrap();
        events.push(armed).unwrap();
        turn(&mut feed, &mut sub, &hold);
        drop(guard);
        events.push(beat(200, 1)).unwrap();
        events.push(beat(300, 0)).unwrap();
        turn(&mut feed, &mut sub, &hold);
        let frames = sent.lock().unwrap();
        assert!(frames[1].catch_up && !frames[1].reset);
        assert_eq!(frames[1].events.iter().map(|e| e.0).collect::<Vec<_>>(), [armed, beat(200, 1), beat(300, 0)]);
    }

    #[test]
    fn an_ack_before_the_first_frame_null_or_past_the_last_covers_nothing() {
        let (mut feed, _) = scripted_feed();
        feed.seq = 10;
        let hold = Hold::default();
        let (mut sub, _) = subscriber();
        turn(&mut feed, &mut sub, &hold);
        let s = sub.as_mut().unwrap();
        for seq in [None, Some(0), Some(9), Some(11)] {
            assert!(!s.ack(seq, Instant::now()));
            assert_eq!(s.in_flight.len(), 1);
            assert_eq!(s.newest_ack, None);
        }
        assert!(!s.ack(Some(10), Instant::now()));
        assert!(s.in_flight.is_empty());
        assert_eq!(s.newest_ack, Some(10));
    }

    #[test]
    fn another_engine_during_a_hold_or_withhold_latches_a_reset_with_its_notices() {
        for held in [false, true] {
            let (mut feed, mut events) = scripted_feed();
            let hold = Arc::new(Hold::default());
            let (mut sub, sent) = subscriber();
            fill_window(&mut feed, &mut sub, &hold, &mut events);
            let guard = held.then(|| FeedHold::take(&hold));
            if held {
                sub.as_mut().unwrap().ack(Some(MAX_IN_FLIGHT as u64 - 1), Instant::now());
            }
            events.push(rejected(100)).unwrap();
            turn(&mut feed, &mut sub, &hold);
            feed.host.core.engine_gen.fetch_add(1, AcqRel);
            turn(&mut feed, &mut sub, &hold);
            turn(&mut feed, &mut sub, &hold);
            drop(guard);
            sub.as_mut().unwrap().ack(Some(MAX_IN_FLIGHT as u64 - 1), Instant::now());
            let current = Event::PassDropped { frame: 200, lane: 1, pass: 2 };
            events.push(current).unwrap();
            turn(&mut feed, &mut sub, &hold);
            let frames = sent.lock().unwrap();
            let reset = frames.last().unwrap();
            assert!(reset.reset && !reset.catch_up && reset.settings.is_some());
            let events: Vec<_> = reset.events.iter().map(|e| e.0).collect();
            assert_eq!(&events[events.len() - 2..], &[rejected(100), current], "kept notices follow the new mirror and precede this tick's");
        }
    }

    #[test]
    fn past_256_withheld_events_a_reset_keeps_take_rejected_and_refused() {
        for total in [WITHHELD_EVENTS, WITHHELD_EVENTS + 1] {
            let (mut feed, mut events) = scripted_feed();
            let hold = Hold::default();
            let (mut sub, sent) = subscriber();
            fill_window(&mut feed, &mut sub, &hold, &mut events);
            let refused = Event::Refused { frame: 101, lane: 0, reason: lf_engine::Refusal::Empty };
            events.push(rejected(100)).unwrap();
            events.push(refused).unwrap();
            for n in 2..total {
                events.push(Event::Selected { frame: n as Frame, lane: 0 }).unwrap();
                events.push(beat(n as Frame, 0)).unwrap();
                turn(&mut feed, &mut sub, &hold);
            }
            sub.as_mut().unwrap().ack(Some(MAX_IN_FLIGHT as u64 - 1), Instant::now());
            turn(&mut feed, &mut sub, &hold);
            let frames = sent.lock().unwrap();
            let last = frames.last().unwrap();
            assert_eq!((last.reset, last.catch_up), (total > WITHHELD_EVENTS, total == WITHHELD_EVENTS));
            assert_eq!(last.events.iter().map(|e| e.0).filter(is_notice).collect::<Vec<_>>(), [rejected(100), refused]);
            assert!(feed.withheld_events.is_empty() && feed.kept_notices.is_empty());
        }
    }

    #[test]
    fn a_withhold_and_a_hold_overlap_until_both_end_even_without_new_events() {
        let (mut feed, mut events) = scripted_feed();
        let hold = Arc::new(Hold::default());
        let (mut sub, sent) = subscriber();
        fill_window(&mut feed, &mut sub, &hold, &mut events);
        turn(&mut feed, &mut sub, &hold);
        let guard = FeedHold::take(&hold);
        sub.as_mut().unwrap().ack(Some(MAX_IN_FLIGHT as u64 - 1), Instant::now());
        turn(&mut feed, &mut sub, &hold);
        assert_eq!(sent.lock().unwrap().len(), MAX_IN_FLIGHT);
        drop(guard);
        turn(&mut feed, &mut sub, &hold);
        turn(&mut feed, &mut sub, &hold);
        let frames = sent.lock().unwrap();
        assert_eq!(frames.len(), MAX_IN_FLIGHT + 1);
        let last = frames.last().unwrap();
        assert!(last.catch_up && !last.reset && last.events.is_empty());
    }

    #[test]
    fn the_watchdog_measures_ack_progress_not_the_age_of_a_frame() {
        let (mut feed, mut events) = scripted_feed();
        let hold = Hold::default();
        let (mut sub, _) = subscriber();
        fill_window(&mut feed, &mut sub, &hold, &mut events);
        let s = sub.as_mut().unwrap();
        let now = Instant::now() + Duration::from_secs(60);
        assert!(!s.ack(None, now), "the first heartbeat after a long stall only starts the watch");
        assert!(!s.ack(None, now + STUCK - Duration::from_millis(1)));
        assert!(s.ack(None, now + STUCK));
        assert!(!s.ack(Some(0), now + Duration::from_secs(6)), "frame zero is progress");
        assert_eq!(s.behind_since, None);
        let later = now + Duration::from_secs(60);
        assert!(!s.ack(Some(0), later));
        assert!(s.ack(Some(0), later + STUCK));
        assert!(!s.ack(Some(1), later + STUCK), "a slow page still advances");
        assert!(!s.ack(Some(0), later + STUCK), "an older ack starts a new watch");
        assert!(s.ack(Some(0), later + STUCK * 2));
        assert!(!s.ack(Some(MAX_IN_FLIGHT as u64 - 1), later + STUCK * 2));
        assert!(s.in_flight.is_empty());
        assert!(!s.ack(None, later + STUCK * 4), "nothing in flight needs no watchdog");
        assert_eq!(s.behind_since, None);
    }

    #[test]
    fn a_new_subscribe_takes_over_unacked_notices_and_its_reset_wins_over_a_hold() {
        let (mut feed, mut events) = scripted_feed();
        let hold = Arc::new(Hold::default());
        let (sub, _) = subscriber();
        let thread = FeedThread {
            subscriber: Arc::new(Mutex::new(sub)), hold: hold.clone(), stop: Arc::new(AtomicBool::new(false)), join: Mutex::new(None),
        };
        events.push(rejected(1)).unwrap();
        turn(&mut feed, &mut thread.subscriber.lock().unwrap(), &hold);
        assert!(!thread.ack(Some(0), Instant::now()));
        let refused = Event::Refused { frame: 3, lane: 1, reason: lf_engine::Refusal::Empty };
        let dropped = Event::PassDropped { frame: 4, lane: 1, pass: 7 };
        for event in [rejected(2), refused, dropped] {
            events.push(event).unwrap();
            turn(&mut feed, &mut thread.subscriber.lock().unwrap(), &hold);
        }
        let guard = thread.hold();
        let sent = Sent::default();
        let sink = sent.clone();
        thread.subscribe(move |frame| { sink.lock().unwrap().push(frame); true });
        events.push(rejected(5)).unwrap();
        turn(&mut feed, &mut thread.subscriber.lock().unwrap(), &hold);
        assert!(sent.lock().unwrap().is_empty());
        drop(guard);
        turn(&mut feed, &mut thread.subscriber.lock().unwrap(), &hold);
        turn(&mut feed, &mut thread.subscriber.lock().unwrap(), &hold);
        let frames = sent.lock().unwrap();
        assert_eq!(frames.len(), 1);
        let reset = &frames[0];
        assert!(reset.reset && !reset.catch_up && reset.seq == 4);
        assert_eq!(reset.events.iter().map(|e| e.0).filter(is_notice).collect::<Vec<_>>(), [rejected(2), refused, dropped, rejected(5)]);
        assert!(!thread.ack(Some(3), Instant::now()), "the old channel cannot ack the replacement's reset");
        assert_eq!(thread.subscriber.lock().unwrap().as_ref().unwrap().in_flight.len(), 1);
    }

    #[test]
    fn resets_keep_only_the_newest_32_notices_without_a_subscriber() {
        let (mut feed, mut events) = scripted_feed();
        let hold = Hold::default();
        let mut nobody = None;
        for n in 0..KEPT_NOTICES + 5 {
            events.push(rejected(n as Frame)).unwrap();
            turn(&mut feed, &mut nobody, &hold);
        }
        let (mut sub, sent) = subscriber();
        turn(&mut feed, &mut sub, &hold);
        let frames = sent.lock().unwrap();
        assert!(frames[0].reset && !frames[0].catch_up);
        assert_eq!(frames[0].events.iter().map(|e| e.0).filter(is_notice).collect::<Vec<_>>(), (5..KEPT_NOTICES + 5).map(|n| rejected(n as Frame)).collect::<Vec<_>>());
    }

    #[test]
    fn repeated_subscribes_keep_only_the_newest_32_unacked_notices() {
        let (mut feed, mut events) = scripted_feed();
        let hold = Hold::default();
        let (mut sub, _) = subscriber();
        for n in 0..KEPT_NOTICES + 5 {
            events.push(rejected(n as Frame)).unwrap();
        }
        turn(&mut feed, &mut sub, &hold);
        // A second replacement before either got a frame must keep the pending takeover too.
        sub = Some(Subscriber::new(Box::new(|_| true), sub.take()));
        let (replacement, sent) = subscriber();
        sub = Some(Subscriber::new(replacement.unwrap().send, sub.take()));
        turn(&mut feed, &mut sub, &hold);
        let frames = sent.lock().unwrap();
        assert!(frames[0].reset && !frames[0].catch_up);
        assert_eq!(frames[0].events.iter().map(|e| e.0).filter(is_notice).collect::<Vec<_>>(), (5..KEPT_NOTICES + 5).map(|n| rejected(n as Frame)).collect::<Vec<_>>());
    }

    #[test]
    fn a_failed_send_keeps_the_unacked_notices_for_the_next_subscriber() {
        let (mut feed, mut events) = scripted_feed();
        let hold = Hold::default();
        let mut sub = Some(Subscriber::new(Box::new(|_| false), None));
        events.push(rejected(100)).unwrap();
        turn(&mut feed, &mut sub, &hold);
        assert!(sub.is_none());
        events.push(rejected(200)).unwrap();
        turn(&mut feed, &mut sub, &hold);
        let (mut sub, sent) = subscriber();
        turn(&mut feed, &mut sub, &hold);
        let frames = sent.lock().unwrap();
        assert_eq!(frames[0].events.iter().map(|e| e.0).filter(is_notice).collect::<Vec<_>>(), [rejected(100), rejected(200)]);
    }

    fn counting_feed() -> Feed {
        let mut feed = Feed::new(EngineHost { core: Arc::new(super::super::Core::new()) });
        // The engine's first count beat precedes its lane's arm.
        mirror_tick(&mut feed, &[beat(100, 4), count_lane(100, (TRACK_COUNT - 1) as u8, LaneState::Recording, true)]);
        feed
    }

    /// The count follows every lane and the selection, before the drain's other one-shots.
    fn assert_count_reset(feed: &Feed, rest: &[Event]) {
        let events: Vec<_> = feed.state_events().into_iter().map(|e| e.0).collect();
        assert_eq!(events.len(), TRACK_COUNT + 1 + rest.len());
        for (i, event) in events[..TRACK_COUNT].iter().enumerate() {
            assert!(matches!(event, Event::Lane { lane, .. } if usize::from(*lane) == i));
        }
        assert_eq!(events[TRACK_COUNT], Event::Selected { frame: 0, lane: 0 });
        assert_eq!(&events[TRACK_COUNT + 1..], rest);
    }

    #[test]
    fn a_reset_between_count_beats_replays_only_the_latest() {
        let mut feed = counting_feed();
        mirror_tick(&mut feed, &[beat(200, 3)]);
        mirror_tick(&mut feed, &[]);
        assert_count_reset(&feed, &[beat(200, 3)]);
    }

    #[test]
    fn a_reset_after_the_count_downbeat_keeps_both_beats_while_armed() {
        let mut feed = counting_feed();
        mirror_tick(&mut feed, &[beat(200, 1)]);
        mirror_tick(&mut feed, &[beat(300, 0)]);
        mirror_tick(&mut feed, &[]);
        assert_count_reset(&feed, &[beat(200, 1), beat(300, 0)]);
        // An ordinary beat does not replace the count's downbeat, or precede its replay.
        mirror_tick(&mut feed, &[beat(400, 0)]);
        assert_count_reset(&feed, &[beat(200, 1), beat(300, 0), beat(400, 0)]);
        mirror_tick(&mut feed, &[]);
        assert_count_reset(&feed, &[beat(200, 1), beat(300, 0)]);
    }

    #[test]
    fn a_reset_after_the_take_started_replays_no_count() {
        let mut feed = counting_feed();
        mirror_tick(&mut feed, &[beat(200, 0), count_lane(210, (TRACK_COUNT - 1) as u8, LaneState::Recording, false)]);
        mirror_tick(&mut feed, &[]);
        assert_count_reset(&feed, &[]);
    }

    #[test]
    fn a_cancelled_count_leaves_nothing_for_a_reset_or_a_later_arm() {
        let mut feed = counting_feed();
        // No downbeat: the lane leaving ARMED alone ends the count.
        mirror_tick(&mut feed, &[count_lane(150, (TRACK_COUNT - 1) as u8, LaneState::Empty, false)]);
        mirror_tick(&mut feed, &[]);
        assert_count_reset(&feed, &[]);
        mirror_tick(&mut feed, &[count_lane(200, (TRACK_COUNT - 1) as u8, LaneState::Recording, true)]);
        assert_count_reset(&feed, &[]);
    }

    #[test]
    fn a_cancelled_count_is_not_replayed_for_another_lanes_ordinary_arm_in_the_same_drain() {
        let mut feed = counting_feed();
        mirror_tick(&mut feed, &[beat(200, 3)]);
        mirror_tick(&mut feed, &[
            count_lane(300, (TRACK_COUNT - 1) as u8, LaneState::Empty, false),
            count_lane(300, 0, LaneState::Recording, true),
        ]);
        mirror_tick(&mut feed, &[]);
        assert_count_reset(&feed, &[]);
    }

    #[test]
    fn a_new_count_at_the_cancel_frame_survives_the_old_lanes_disarm() {
        let mut feed = counting_feed();
        mirror_tick(&mut feed, &[beat(200, 3)]);
        mirror_tick(&mut feed, &[
            beat(300, 4),
            count_lane(300, (TRACK_COUNT - 1) as u8, LaneState::Empty, false),
            count_lane(300, 0, LaneState::Recording, true),
        ]);
        mirror_tick(&mut feed, &[]);
        assert_count_reset(&feed, &[beat(300, 4)]);
    }

    #[test]
    fn a_cancelled_count_downbeat_is_not_replayed_for_another_lanes_ordinary_arm() {
        let mut feed = counting_feed();
        mirror_tick(&mut feed, &[beat(200, 1), beat(300, 0)]);
        mirror_tick(&mut feed, &[
            count_lane(400, (TRACK_COUNT - 1) as u8, LaneState::Empty, false),
            count_lane(400, 0, LaneState::Recording, true),
        ]);
        mirror_tick(&mut feed, &[]);
        assert_count_reset(&feed, &[]);
    }

    #[test]
    fn count_beats_drained_in_the_reset_tick_are_sent_once() {
        let mut feed = counting_feed();
        assert_count_reset(&feed, &[beat(100, 4)]);
        mirror_tick(&mut feed, &[beat(200, 3)]);
        assert_count_reset(&feed, &[beat(200, 3)]);
        mirror_tick(&mut feed, &[beat(300, 0)]);
        assert_count_reset(&feed, &[beat(200, 3), beat(300, 0)]);
        mirror_tick(&mut feed, &[beat(400, 1), beat(500, 0)]);
        assert_count_reset(&feed, &[beat(400, 1), beat(500, 0)]);
    }

    #[test]
    fn a_later_count_replaces_the_old_beat_and_downbeat() {
        let mut feed = counting_feed();
        mirror_tick(&mut feed, &[beat(200, 0)]);
        mirror_tick(&mut feed, &[beat(300, 4)]);
        mirror_tick(&mut feed, &[]);
        assert_count_reset(&feed, &[beat(300, 4)]);
    }

    #[test]
    fn another_engine_drops_the_count_even_if_it_reports_an_armed_lane() {
        let mut feed = counting_feed();
        mirror_tick(&mut feed, &[beat(200, 0)]);
        feed.drained = vec![count_lane(0, (TRACK_COUNT - 1) as u8, LaneState::Recording, true)];
        assert!(feed.update_mirror(1), "a new engine resets the feed");
        assert_count_reset(&feed, &[]);
        assert!(feed.count_beat.is_none() && feed.count_downbeat.is_none());
    }

    #[test]
    fn a_reload_mid_count_replays_the_counted_lanes_arm_and_the_counts_latest_beat() {
        let (mut engine, mut feed) = manual_engine();
        let (mut page, _) = subscriber();
        let hold = Hold::default();
        let mut at = 0;
        // A beat is 2000 frames: the count beats on 0, 2000, 4000 and 6000, its downbeat on 8000.
        send(&feed, [Command::SetBpm(240.0), Command::RecDub(0)]);
        render(&mut engine, &mut at, 1);
        turn(&mut feed, &mut page, &hold);
        assert_eq!(replayed_count(&reload(&mut feed)), (vec![(0, 4)], vec![0]), "before the second beat");
        render(&mut engine, &mut at, 15);
        turn(&mut feed, &mut page, &hold);
        assert_eq!(replayed_count(&reload(&mut feed)), (vec![(2000, 3)], vec![0]), "after it");
        render(&mut engine, &mut at, 70);
        turn(&mut feed, &mut page, &hold);
        assert_eq!(replayed_count(&reload(&mut feed)), (vec![], vec![]), "the take records: no count");
    }

    #[test]
    fn a_counts_first_beat_drained_before_its_lanes_arm_is_still_replayed() {
        for later in [false, true] {
            let (mut engine, mut feed) = manual_engine();
            let (mut page, _) = subscriber();
            let hold = Hold::default();
            let mut at = 0;
            send(&feed, [Command::SetBpm(240.0)]);
            render(&mut engine, &mut at, 1);
            turn(&mut feed, &mut page, &hold);
            let lane = if later {
                idle_master(&mut engine, &mut feed, &mut page, &mut at);
                1
            } else {
                0
            };
            send(&feed, [Command::RecDub(lane)]);
            render(&mut engine, &mut at, 1);
            // The beat is pushed before the publish of its lane's arm, in the same chunk.
            let first = drain_through_count_beat(&mut feed);
            assert_eq!(first, (at - 128, 4), "the count starts at the press");
            turn(&mut feed, &mut page, &hold);
            assert_eq!(replayed_count(&reload(&mut feed)), (vec![first], vec![lane]), "later take: {later}");
        }
    }

    #[test]
    fn a_beat_on_a_cancels_frame_is_not_replayed_as_a_lone_downbeat_for_another_lanes_arm() {
        let (mut engine, mut feed) = manual_engine();
        let (mut page, _) = subscriber();
        let hold = Hold::default();
        let mut at = 0;
        send(&feed, [Command::SetBpm(240.0)]);
        render(&mut engine, &mut at, 1);
        turn(&mut feed, &mut page, &hold);
        idle_master(&mut engine, &mut feed, &mut page, &mut at);
        // A later take counted in from the idle transport, past its second beat.
        send(&feed, [Command::RecDub(1)]);
        render(&mut engine, &mut at, 20);
        turn(&mut feed, &mut page, &hold);
        let (beats, armed) = replayed_count(&reload(&mut feed));
        assert_eq!((beats.len(), beats[0].1, armed), (1, 3, vec![1]));
        // The count cancelled and the transport started in one block (its grid re-anchors on the press,
        // so a beat lands there), then another lane armed for the next boundary: one drain takes both.
        send(&feed, [Command::Stop(1), Command::PlayAll]);
        render(&mut engine, &mut at, 1);
        let cancel = at - 128;
        send(&feed, [Command::RecDub(2)]);
        render(&mut engine, &mut at, 1);
        turn(&mut feed, &mut page, &hold);
        assert!(feed.drained.iter().any(|e| matches!(*e, Event::Beat { frame, count_left: 0, .. } if frame == cancel)), "a beat on the cancel's frame");
        assert_eq!(replayed_count(&reload(&mut feed)), (vec![], vec![2]));
    }

    #[test]
    fn a_held_feed_sends_nothing_keeps_its_mirror_and_ends_with_one_catch_up() {
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
        drop(guard);
        turn(&mut feed, &mut sub, &hold);
        {
            let sent = sent.lock().unwrap();
            assert_eq!(sent.len(), 3, "one frame ends the hold");
            let frame = &sent[2];
            assert!(frame.catch_up && !frame.reset && frame.settings.is_none(), "the hold ends with a catch-up");
            assert_eq!(frame.seq, 2, "the held ticks built no frame: the count goes on where it stopped");
            assert_eq!(tempo(frame), Some(240), "the event drained while held is replayed");
        }
        set_bpm(&device, 120.0);
        for _ in 0..10 {
            turn(&mut feed, &mut sub, &hold);
        }
        let sent = sent.lock().unwrap();
        assert_eq!((sent.len(), sent.iter().filter(|f| f.catch_up).count()), (4, 1), "then ordinary frames again, and no second catch-up");
        assert_eq!((sent[3].seq, tempo(&sent[3])), (3, Some(120)));
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
    fn a_hold_released_at_once_sends_exactly_one_catch_up() {
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
        assert!(sent[1].catch_up && !sent[1].reset && sent[1].seq == 1);
    }

    /// Two overlapping holds end when the last one drops; a guard dropped by an early return (an
    /// error on its way out with `?`) releases like any other.
    #[test]
    fn the_guard_releases_on_an_early_return_and_the_last_of_two_sends_a_catch_up() {
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
        assert_eq!(counts(&sent), (2, 1), "and the feed catches up");
        assert!(sent.lock().unwrap()[1].catch_up);

        let (first, second) = (FeedHold::take(&hold), FeedHold::take(&hold));
        drop(first);
        set_bpm(&device, 240.0);
        turn(&mut feed, &mut sub, &hold);
        assert_eq!(sent.lock().unwrap().len(), 2, "still held by the second");
        drop(second);
        turn(&mut feed, &mut sub, &hold);
        assert_eq!(counts(&sent), (3, 1));
        assert!(sent.lock().unwrap()[2].catch_up);
    }

    /// The scope taps on the feed (`lf_engine::scope`): nothing until the UI asks for them, then
    /// batches that splice onto each other by frame, a `gap` wherever the trace broke (the first batch,
    /// the columns a held tick dropped), and no columns on a reset frame.
    #[test]
    fn the_feed_carries_scope_columns_and_says_where_the_trace_broke() {
        // A slow device: a column (192 frames at 48 kHz) takes three blocks, so a tick's batch stays
        // far under the cap however the test machine schedules this thread.
        let device = TestDevice::start(48_000, 64, Duration::from_millis(10), |_| 0.0);
        assert!(device.wait_blocks(4), "the engine renders");
        let mut feed = Feed::new(device.host().clone());
        let hold = Arc::new(Hold::default());
        let (mut sub, sent) = subscriber();
        let batches = |sent: &Sent| sent.lock().unwrap().iter().filter_map(|f| f.scope.clone()).collect::<Vec<_>>();
        let columns = |batch: &ScopeUpdate| batch.min[0].len();
        turn(&mut feed, &mut sub, &hold);
        assert!(device.wait_blocks(12));
        turn(&mut feed, &mut sub, &hold);
        assert!(batches(&sent).is_empty(), "nobody asked for the scope");

        device.host().send(TimedCommand { frame: None, command: Command::SetScope(true) }).unwrap();
        assert!(device.wait_blocks(12));
        turn(&mut feed, &mut sub, &hold);
        let first = batches(&sent).pop().expect("the columns reach the feed");
        assert_eq!((first.bin, first.min.len(), first.max.len()), (192, SCOPE_SOURCES, SCOPE_SOURCES));
        assert!(first.min.iter().chain(&first.max).all(|c| c.len() == columns(&first)), "every array holds the same columns");
        assert!(first.gap, "the first batch has nothing to splice onto");

        assert!(device.wait_blocks(9));
        turn(&mut feed, &mut sub, &hold);
        let next = batches(&sent).pop().expect("a second batch");
        assert!(!next.gap, "it splices onto the one before it");
        assert_eq!(next.frame, first.frame + columns(&first) as Frame * first.bin as Frame, "no hole and no overlap between batches");

        // A hole inside one drain (the taps switched off and on between two ticks): only the newest run
        // goes out, and it says the trace broke. Were the whole drain sent, its `frame` would be where
        // the batch above ended and its `bin` would claim the hole never happened.
        let spliced = next.frame + columns(&next) as Frame * next.bin as Frame;
        assert!(device.wait_blocks(12), "columns before the switch");
        device.host().send(TimedCommand { frame: None, command: Command::SetScope(false) }).unwrap();
        assert!(device.wait_blocks(30), "the hole: frames no column covers");
        // A block already rendered: the taps switch on at a later frame, whatever the test thread's lag.
        let (switched_on, _, _) = device.host().core.clock.anchor().expect("the device has rendered");
        device.host().send(TimedCommand { frame: None, command: Command::SetScope(true) }).unwrap();
        assert!(device.wait_blocks(12), "columns after it");
        turn(&mut feed, &mut sub, &hold);
        let run = batches(&sent).pop().expect("a batch after the hole");
        assert!(run.gap, "a batch that cannot be spliced says so");
        assert!(run.frame > spliced, "it is the run after the hole, not the one before it ({} against {spliced})", run.frame);
        assert!(run.frame >= switched_on, "the run before the hole is not in it ({} against {switched_on})", run.frame);

        // Held, the columns are drained and dropped: the catch-up's batch says the trace broke.
        let guard = FeedHold::take(&hold);
        assert!(device.wait_blocks(12));
        for _ in 0..4 {
            turn(&mut feed, &mut sub, &hold);
        }
        let sent_while_held = batches(&sent).len();
        drop(guard);
        // Columns waiting when the hold ends reach the catch-up with a gap.
        assert!(device.wait_blocks(9));
        turn(&mut feed, &mut sub, &hold);
        {
            let frames = sent.lock().unwrap();
            let catch_up = frames.last().expect("the hold ends with a catch-up");
            assert!(catch_up.catch_up && !catch_up.reset);
            assert!(catch_up.scope.as_ref().is_some_and(|s| s.gap), "the columns the hold dropped broke the trace");
        }
        assert!(device.wait_blocks(9));
        turn(&mut feed, &mut sub, &hold);
        let after = batches(&sent);
        assert_eq!(after.len(), sent_while_held + 2, "the catch-up and the following tick each carry a batch");
        assert!(!after[after.len() - 1].gap, "the following batch splices onto the catch-up");
    }

    /// The thread itself: held, it sends nothing while the engine reports; released, its next frame
    /// is a catch-up; and `stop` ends it while it is held.
    #[test]
    fn the_feed_thread_holds_catches_up_and_still_stops_while_held() {
        let wait_for = |pred: &dyn Fn() -> bool| {
            let deadline = Instant::now() + PATIENCE;
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
        assert!(wait_for(&|| sent.lock().unwrap().len() >= 2), "the hold's end sends a frame");
        {
            let sent = sent.lock().unwrap();
            assert!(sent[1].catch_up && !sent[1].reset && sent[1].seq == 1 && tempo(&sent[1]) == Some(240));
        }

        let _held = thread.hold();
        let (done, stopped) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                thread.stop();
                let _ = done.send(());
            });
            assert!(stopped.recv_timeout(PATIENCE).is_ok(), "a held feed thread still stops");
        });
    }

    #[test]
    fn the_feed_thread_still_stops_while_withheld() {
        let (feed, mut events) = scripted_feed();
        let thread = FeedThread::spawn(feed.host.clone()).unwrap();
        let (sent, received) = std::sync::mpsc::channel();
        thread.subscribe(move |frame| sent.send(frame).is_ok());
        assert!(received.recv_timeout(PATIENCE).unwrap().reset);
        for n in 1..MAX_IN_FLIGHT {
            events.push(Event::Selected { frame: n as Frame, lane: 0 }).unwrap();
            assert_eq!(received.recv_timeout(PATIENCE).unwrap().seq, n as u64);
        }
        let deadline = Instant::now() + PATIENCE;
        while !thread.subscriber.lock().unwrap().as_ref().unwrap().withheld && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(thread.subscriber.lock().unwrap().as_ref().unwrap().withheld);
        assert!(received.try_recv().is_err(), "the full window sent no extra frame");
        let (done, stopped) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                thread.stop();
                let _ = done.send(());
            });
            assert!(stopped.recv_timeout(PATIENCE).is_ok(), "a withheld feed thread still stops");
        });
    }
}
