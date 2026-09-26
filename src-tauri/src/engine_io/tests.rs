//! The device owner and the callbacks on the fake driver (`fake_driver.rs`): no hardware, the real
//! engine. Each test drives an [`EngineHost`] as its callers will (open, switch, close, the slot
//! handshake) and reads what the fake played, the counters and the device events.

use std::any::Any;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::{Relaxed, SeqCst}};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use lf_engine::grid::Frame;
use lf_engine::{Command, Event, LaneInfo, LaneState, SlotEvent, SlotKind, SlotProcessor, TimedCommand};

use super::callback::Side;
use super::fake_driver::{Fake, FakeDevice, FakeDriver};
use super::owner::Request;
use super::{DeviceEvent, DeviceRequest, DeviceStatus, EngineHost, HostConfig};
use crate::audio_output::AudioBackend;

/// The longest any wait here takes: the fake plays faster than real time, so a wait that runs this long
/// is a failure, not a slow machine.
const PATIENCE: Duration = Duration::from_secs(20);
const RATE: Frame = 48_000;

/// One fake device renders at a time: each runs faster than real time, and a dozen at once starve the
/// other tests' threads (the VST3 restart fixture's reader drops hop-1 frames). It also keeps
/// `rt_allocs`, a process-wide counter, to one test's callbacks.
static SERIAL: Mutex<()> = Mutex::new(());

fn asio(buffer: Option<u32>) -> DeviceRequest {
    DeviceRequest { backend: AudioBackend::Asio, input: None, output: None, input_channel: None, buffer }
}

fn wasapi(input: Option<&str>, output: Option<&str>) -> DeviceRequest {
    DeviceRequest {
        backend: AudioBackend::Wasapi,
        input: input.map(str::to_string),
        output: output.map(str::to_string),
        input_channel: None,
        buffer: None,
    }
}

/// An engine host on the fake driver; shut down on drop, even when a test fails.
struct Harness {
    host: EngineHost,
    fake: Arc<Fake>,
    events: Vec<Event>,
    /// Where the next `wait_event` looks from: just past the event the last one found.
    cursor: usize,
    _serial: MutexGuard<'static, ()>,
}

impl Harness {
    fn new() -> Harness {
        let serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let fake = Fake::new();
        let host = EngineHost::with_driver(HostConfig { max_loop_seconds: 8.0 }, FakeDriver(fake.clone()));
        Harness { host, fake, events: Vec::new(), cursor: 0, _serial: serial }
    }

    fn open(&self, request: DeviceRequest) -> DeviceStatus {
        self.host.open(request).expect("the fake device opens")
    }

    fn send(&self, command: Command) {
        self.host.send(TimedCommand { frame: None, command }).expect("the command ring takes it");
    }

    /// The device frame the next callback renders.
    fn frame(&self) -> Frame {
        self.host.core.frame.load(Relaxed)
    }

    /// Wait until the device has played `frames` more.
    fn play(&self, frames: Frame) {
        let target = self.frame() + frames;
        until("the device plays on", || self.frame() >= target);
    }

    /// Wait for the next engine event that `pick` accepts, after the one the last wait found.
    fn wait_event<T>(&mut self, what: &str, mut pick: impl FnMut(&Event) -> Option<T>) -> T {
        let deadline = Instant::now() + PATIENCE;
        loop {
            self.host.drain_events(&mut self.events);
            while self.cursor < self.events.len() {
                let found = pick(&self.events[self.cursor]);
                self.cursor += 1;
                if let Some(found) = found {
                    return found;
                }
            }
            assert!(Instant::now() < deadline, "no event: {what}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn wait_lane(&mut self, what: &str, lane: u8, want: impl Fn(&LaneInfo) -> bool) -> LaneInfo {
        self.wait_event(what, |e| match e {
            Event::Lane { lane: l, info, .. } if *l == lane && want(info) => Some(*info),
            _ => None,
        })
    }

    /// Record a one-bar loop of the input on lane 0 at 240 BPM (a bar is a second) and wait until it
    /// plays: its length. The take records the wet signal, so slot 0 goes live (an empty slot passes the
    /// input dry) for the take only; what plays after is the loop alone.
    fn record_loop(&mut self) -> Frame {
        self.send(Command::SetBpm(240.0));
        self.send(Command::SetSlotLive(0, true));
        self.send(Command::RecDub(0));
        self.wait_lane("the take starts after the count-in", 0, |i| i.state == LaneState::Recording && !i.armed);
        self.play(RATE * 3 / 2);
        self.send(Command::RecDub(0));
        let length = self.wait_lane("the loop plays", 0, |i| i.state == LaneState::Playing && i.length > 0).length;
        self.send(Command::SetSlotLive(0, false));
        length
    }

    /// The device events so far, waiting (bounded) until there are `n`.
    fn device_events(&self, n: usize) -> Vec<DeviceEvent> {
        let mut events = Vec::new();
        let deadline = Instant::now() + PATIENCE;
        while events.len() < n && Instant::now() < deadline {
            events.extend(self.host.take_device_events());
            std::thread::sleep(Duration::from_millis(2));
        }
        events
    }

    /// The frame each output callback started at, one list per run.
    fn starts(&self) -> Vec<Vec<Frame>> {
        self.fake.starts.lock().unwrap().clone()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.host.shutdown();
    }
}

fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !done() {
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Past its fade-in, run `run` plays exactly what the loop played one `length` earlier: the loop
/// resumed in place.
fn assert_resumed_in_place(h: &Harness, run: usize, length: Frame) {
    let from = h.starts()[run][0] + RATE / 20;
    let after = h.fake.heard(from..from + 4_000);
    let before = h.fake.heard(from - length..from - length + 4_000);
    assert!(after.iter().any(|s| s.abs() > 0.05), "the loop is heard in run {run}");
    let diff = max_diff(&after, &before);
    assert!(diff < 1e-4, "run {run} resumed the loop in place (diff {diff})");
}

/// The largest difference between two stretches of the tape (NaN, nothing played, counts as a miss).
fn max_diff(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| if x.is_nan() || y.is_nan() { f32::INFINITY } else { (x - y).abs() }).fold(0.0, f32::max)
}

/// A sine that repeats on no loop length, so a loop played at the wrong phase shows; quiet enough (0.2 on
/// the fake's input 2) that the master limiter leaves it alone.
fn tone(frame: Frame) -> f32 {
    0.1 * (frame as f32 * 0.013_7).sin()
}

/// A plugin stand-in: an effect outputting a constant, counting its calls and stops; it panics on the
/// call numbered `panic_at`, if one is set, and in `stop` if `panic_on_stop`.
struct Unit {
    level: f32,
    calls: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
    panic_at: Option<usize>,
    panic_on_stop: bool,
}

impl Unit {
    fn new(level: f32) -> (Box<Unit>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let (calls, stops) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        (Box::new(Unit { level, calls: calls.clone(), stops: stops.clone(), panic_at: None, panic_on_stop: false }), calls, stops)
    }
}

impl SlotProcessor for Unit {
    fn kind(&self) -> SlotKind {
        SlotKind::Effect
    }
    fn latency(&self) -> Frame {
        0
    }
    fn process(&mut self, _frame: Frame, _input: &[f32], _events: &[SlotEvent], out: &mut [f32]) {
        let call = self.calls.fetch_add(1, SeqCst) + 1;
        if self.panic_at == Some(call) {
            panic!("the unit fails on call {call} (on purpose)");
        }
        out.fill(self.level);
    }
    fn stop(&mut self) {
        self.stops.fetch_add(1, SeqCst);
        if self.panic_on_stop {
            panic!("the unit fails to stop (on purpose)");
        }
    }
    fn into_any(self: Box<Self>) -> Box<dyn Any + Send> {
        self
    }
}

#[test]
fn a_loop_resumes_in_place_across_a_switch_and_the_close_stops_the_device() {
    let mut h = Harness::new();
    h.fake.set_input(tone);
    let status = h.open(asio(Some(256)));
    assert_eq!((status.backend, status.sample_rate, status.block), (AudioBackend::Asio, 48_000, 256));
    let length = h.record_loop();
    assert_eq!(length, RATE, "one bar at 240 BPM");
    h.play(2 * length);

    let switched = h.open(asio(Some(128)));
    assert_eq!(switched.block, 128);
    h.play(length / 2);
    h.host.close().unwrap();
    assert_eq!(h.host.status(), None);
    let frame = h.frame();
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(h.frame(), frame, "no callback after the close");

    let starts = h.starts();
    assert_eq!(starts.len(), 2, "one run per device");
    let (first, second) = (&starts[0], &starts[1]);
    assert!(first.windows(2).all(|w| w[1] - w[0] == 256), "the first run's frames are contiguous");
    assert!(second.windows(2).all(|w| w[1] - w[0] == 128), "the second run's frames are contiguous");
    assert_eq!(second[0], first.last().unwrap() + 256, "the frame counter carries over the switch");
    let last = *first.last().unwrap();
    assert!(
        h.fake.heard(last - 256..last + 256).iter().all(|&s| s == 0.0),
        "the old device's last two blocks are silent (a dropped ASIO stream keeps playing its two buffers)"
    );
    let ramp = h.fake.heard(second[0]..second[0] + 480);
    assert!(ramp[0].abs() < 0.001 && ramp.iter().any(|s| s.abs() > 0.05), "the new device fades in");
    assert_resumed_in_place(&h, 1, length);
    let diag = h.host.diag();
    assert_eq!((diag.lock_misses, diag.gaps, diag.duplex_faults, diag.panics), (0, 0, 0, 0));
}

#[test]
fn a_take_recording_when_the_device_switches_punches_out_and_is_kept() {
    let mut h = Harness::new();
    h.fake.set_input(tone);
    h.open(asio(Some(256)));
    h.send(Command::SetBpm(240.0));
    h.send(Command::SetSlotLive(0, true));
    h.send(Command::RecDub(0));
    h.wait_lane("the take starts", 0, |i| i.state == LaneState::Recording && !i.armed);
    h.play(RATE * 3 / 2);
    h.open(asio(Some(128)));
    let switched = h.starts()[1][0];
    let (frame, kept) = h.wait_event("the take leaves recording", |e| match e {
        Event::Lane { frame, lane: 0, info } if info.state != LaneState::Recording => Some((*frame, *info)),
        _ => None,
    });
    // Not at the lane's capacity later on (a take nobody stops commits there): at the switch.
    assert!(frame <= switched, "punched out at the switch ({switched}), not at {frame}");
    assert!(kept.length > 0, "the punched-out take is kept as a loop: {kept:?}");
}

#[test]
fn a_lost_asio_device_is_rebuilt_from_its_cache_and_the_loop_carries_on() {
    let mut h = Harness::new();
    h.fake.set_input(tone);
    h.open(asio(Some(256)));
    let length = h.record_loop();
    h.play(2 * length);
    h.fake.fatal.store(Side::Output as u8, SeqCst);
    let events = h.device_events(2);
    assert!(matches!(&events[0], DeviceEvent::Lost { backend: AudioBackend::Asio, reason } if reason.contains("not available")), "{events:?}");
    match &events[1] {
        DeviceEvent::Recovered(status) => assert_eq!((status.backend, status.block), (AudioBackend::Asio, 256)),
        other => panic!("expected a recovery, got {other:?}"),
    }
    h.play(RATE / 2);
    let starts = h.starts();
    assert_eq!(starts[1][0], starts[0].last().unwrap() + 256, "the frame counter carries over the loss");
    assert_resumed_in_place(&h, 1, length);
    assert_eq!(h.host.status().map(|s| s.backend), Some(AudioBackend::Asio));
}

#[test]
fn a_lost_asio_device_that_cannot_come_back_falls_back_to_the_wasapi_defaults() {
    let h = Harness::new();
    h.open(asio(Some(256)));
    h.play(RATE / 10);
    h.fake.fail_starts.store(1, SeqCst);
    h.fake.fatal.store(Side::Output as u8, SeqCst);
    let events = h.device_events(2);
    assert!(matches!(events[0], DeviceEvent::Lost { backend: AudioBackend::Asio, .. }), "{events:?}");
    match &events[1] {
        DeviceEvent::Fallback(status) => assert_eq!(status.backend, AudioBackend::Wasapi),
        other => panic!("expected a fallback, got {other:?}"),
    }
    h.play(RATE / 10);
    assert_eq!(h.host.diag().join_starves, 0);
}

#[test]
fn a_lost_wasapi_endpoint_falls_back_to_the_default_endpoint() {
    let h = Harness::new();
    h.fake.wasapi.lock().unwrap().push(("usb".into(), FakeDevice::new("USB interface", 48_000, 480)));
    h.open(wasapi(None, Some("usb")));
    h.play(RATE / 10);
    h.fake.fatal.store(Side::Output as u8, SeqCst);
    let events = h.device_events(2);
    match &events[1] {
        DeviceEvent::Fallback(status) => assert_eq!(status.output_name, "Fake WASAPI"),
        other => panic!("expected a fallback, got {other:?}: {events:?}"),
    }
}

#[test]
fn a_new_rate_hands_the_units_to_their_owners_and_the_new_engine_takes_them_back() {
    let h = Harness::new();
    h.open(asio(Some(256)));
    let slot = h.host.slot(0);
    let (unit, calls, stops) = Unit::new(0.1);
    assert!(slot.install(unit, RATE as u32).is_ok());
    until("the unit renders", || calls.load(SeqCst) > 0);

    h.fake.asio.lock().unwrap().as_mut().unwrap().rate = 44_100;
    let status = h.open(asio(Some(128)));
    assert_eq!(status.sample_rate, 44_100);
    assert_eq!(slot.rate(), Some(44_100), "what the owner re-activates the unit at");
    let back = slot.take_evicted().expect("the evicted unit waits for its owner");
    assert_eq!(stops.load(SeqCst), 1, "stopped once, before it left");
    let rendered = calls.load(SeqCst);
    assert!(slot.install(back, 44_100).is_ok(), "the new engine takes it");
    until("the new engine renders the unit", || calls.load(SeqCst) > rendered);
    let back = slot.remove(Duration::from_secs(2)).unwrap().expect("the unit comes back");
    assert_eq!(back.into_any().downcast::<Unit>().unwrap().level, 0.1);
    assert_eq!(h.host.diag().lock_misses, 0);
}

#[test]
fn a_plugin_owner_installs_removes_and_reinstalls_while_the_engine_renders() {
    let h = Harness::new();
    h.open(asio(Some(256)));
    let slot = h.host.slot(1);
    for round in 1..=3 {
        let (unit, calls, stops) = Unit::new(0.2);
        assert!(slot.install(unit, RATE as u32).is_ok(), "round {round}");
        until("the unit renders", || calls.load(SeqCst) > 4);
        let callbacks = h.host.diag().callbacks;
        let back = slot.remove(Duration::from_secs(2)).unwrap().expect("the unit comes back");
        assert_eq!(stops.load(SeqCst), 1);
        drop(back);
        assert!(h.host.diag().callbacks > callbacks, "the engine kept rendering through the removal");
    }
    let run = &h.starts()[0];
    assert!(run.windows(2).all(|w| w[1] - w[0] == 256), "no block went missing");
    assert_eq!(h.host.diag().lock_misses, 0);
}

#[test]
fn a_panicking_unit_faults_the_engine_and_the_owner_replaces_it_at_the_same_rate() {
    let h = Harness::new();
    h.open(asio(Some(256)));
    let slot = h.host.slot(0);
    let (mut unit, calls, stops) = Unit::new(0.5);
    unit.panic_at = Some(50);
    h.send(Command::SetSlotLive(0, true));
    assert!(slot.install(unit, RATE as u32).is_ok());
    until("the unit panics", || h.host.diag().panics == 1);
    let events = h.device_events(1);
    assert!(matches!(events[..], [DeviceEvent::EngineFaulted]), "{events:?}");
    let back = slot.take_evicted().expect("the unit waits for its owner");
    assert_eq!((calls.load(SeqCst), stops.load(SeqCst)), (50, 1), "not called again; stopped on its way out");
    drop(back);
    until("the device runs again", || h.host.status().is_some());
    assert_eq!(h.host.status().unwrap().sample_rate, 48_000);
    h.play(RATE / 20);
    assert_eq!(h.host.diag().panics, 1);
    #[cfg(debug_assertions)]
    assert!(h.host.diag().rt_allocs > 0, "the guard counts: the panic's payload allocated under it");
    assert!(!h.host.core.rt.is_poisoned());
    h.host.close().unwrap();
}

#[test]
fn a_unit_that_panics_in_the_idle_slot_service_faults_the_engine_without_poisoning_the_lock() {
    let h = Harness::new();
    h.open(asio(Some(256)));
    let slot = h.host.slot(0);
    let (mut unit, calls, _) = Unit::new(0.1);
    unit.panic_on_stop = true;
    assert!(slot.install(unit, RATE as u32).is_ok());
    until("the unit renders", || calls.load(SeqCst) > 0);
    h.host.close().unwrap();
    // No device runs: the removal is serviced on this thread, and the unit's stop panics there.
    assert!(slot.remove(Duration::from_millis(200)).is_err(), "a unit that cannot stop does not come back");
    assert!(!h.host.core.rt.is_poisoned());
    assert!(h.host.diag().panics >= 1);
    let events = h.device_events(1);
    assert!(matches!(events[..], [DeviceEvent::EngineFaulted]), "{events:?}");
    h.open(asio(Some(256)));
    h.play(RATE / 20);
    assert_eq!(h.host.diag().lock_misses, 0, "the new engine renders");
}

#[test]
fn a_unit_activated_at_a_replaced_rate_comes_back_for_re_activation() {
    let h = Harness::new();
    h.open(asio(Some(256)));
    let slot = h.host.slot(0);
    let (unit, calls, _) = Unit::new(0.1);
    assert!(slot.install(unit, 44_100).is_ok(), "handed on, not refused");
    let back = slot.take_evicted().expect("back for re-activation at the engine's rate");
    assert_eq!(calls.load(SeqCst), 0, "never rendered at the wrong rate");
    assert!(slot.install(back, RATE as u32).is_ok());
    until("the unit renders", || calls.load(SeqCst) > 0);
}

#[test]
fn a_shutdown_hands_an_installed_unit_back_to_its_owner_stopped() {
    let h = Harness::new();
    h.open(asio(Some(256)));
    let slot = h.host.slot(1);
    let (unit, calls, stops) = Unit::new(0.1);
    assert!(slot.install(unit, RATE as u32).is_ok());
    until("the unit renders", || calls.load(SeqCst) > 0);
    h.host.shutdown();
    let back = slot.remove(Duration::from_millis(100)).unwrap().expect("the unit comes back");
    assert_eq!(stops.load(SeqCst), 1, "stopped once, before the engine dropped");
    drop(back);
}

/// Install a unit on a new handle for slot 0, then give it up as a plugin owner's teardown does when
/// `remove` times out: the test holds the engine lock, so no block services the removal in time.
/// Returns the unit's call count (the unit holds a clone: a count of 2 means it was never dropped) and
/// its stop count.
fn abandon_a_unit(h: &Harness) -> (Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let first = h.host.slot(0);
    let (orphan, calls, stops) = Unit::new(0.1);
    assert!(first.install(orphan, RATE as u32).is_ok());
    until("the unit renders", || calls.load(SeqCst) > 0);
    let _held = h.host.core.rt.lock().unwrap();
    assert!(first.remove(Duration::from_millis(20)).is_err(), "no block runs while the lock is held");
    first.abandon();
    (calls, stops)
}

#[test]
fn an_abandoned_unit_leaks_once_it_is_out_and_the_slot_frees_for_the_next_owner() {
    let h = Harness::new();
    h.open(asio(Some(256)));
    let (calls, stops) = abandon_a_unit(&h);
    until("the engine takes the orphan out", || stops.load(SeqCst) == 1);
    // It stops the unit, then hands it back: a few more blocks and it is on the port.
    h.play(RATE / 50);
    let next = h.host.slot(0);
    assert!(next.remove(Duration::from_millis(20)).unwrap().is_none(), "nothing of the next owner's is in the slot");
    let (unit, _, _) = Unit::new(0.3);
    assert!(next.install(unit, RATE as u32).is_ok(), "the slot frees once the orphan is out");
    let back = next.remove(Duration::from_secs(2)).unwrap().expect("the next owner's own unit comes back");
    assert_eq!(back.into_any().downcast::<Unit>().unwrap().level, 0.3);
    assert_eq!(Arc::strong_count(&calls), 2, "the orphan leaked, never dropped");
}

#[test]
fn an_abandoned_unit_a_new_rate_evicts_never_reaches_the_next_owner() {
    let h = Harness::new();
    h.open(asio(Some(256)));
    let (calls, _) = abandon_a_unit(&h);
    let next = h.host.slot(0);
    h.fake.asio.lock().unwrap().as_mut().unwrap().rate = 44_100;
    h.open(asio(Some(128)));
    assert!(next.take_evicted().is_none(), "the orphan is no one's to take back");
    let (unit, _, _) = Unit::new(0.3);
    assert!(next.install(unit, 44_100).is_ok(), "the eviction freed the slot");
    assert_eq!(Arc::strong_count(&calls), 2, "the orphan leaked, never dropped");
}

/// `Request::Open` as `EngineHost::open` sends it, with the caller's claim in the test's hand.
fn open_claimed(h: &Harness, request: DeviceRequest, claimed: &Arc<AtomicBool>) -> Result<DeviceStatus, String> {
    let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
    h.host.owner_tx().unwrap().send(Request::Open(request, claimed.clone(), reply_tx)).unwrap();
    reply_rx.recv_timeout(PATIENCE).expect("the owner answers")
}

#[test]
fn a_channel_change_its_caller_gave_up_on_before_it_ran_leaves_the_device_running() {
    let h = Harness::new();
    h.open(asio(Some(256)));
    let claimed = Arc::new(AtomicBool::new(true));
    assert!(open_claimed(&h, DeviceRequest { input_channel: Some(1), ..asio(Some(256)) }, &claimed).is_err());
    assert_eq!(h.host.status().map(|s| s.block), Some(256), "the device still runs");
    let frame = h.frame();
    h.play(RATE / 20);
    assert!(h.frame() > frame);
    assert_eq!(h.fake.started.load(SeqCst), 1);
}

#[test]
fn a_switch_its_caller_gave_up_on_midway_puts_back_the_device_that_ran() {
    let h = Harness::new();
    h.open(asio(Some(256)));
    let claimed = Arc::new(AtomicBool::new(false));
    let gave_up = claimed.clone();
    *h.fake.on_start.lock().unwrap() = Some(Box::new(move || gave_up.store(true, SeqCst)));
    assert!(open_claimed(&h, asio(Some(128)), &claimed).is_err(), "the caller hears the open was cancelled");
    assert_eq!(h.host.status().map(|s| s.block), Some(256), "the device that ran before runs again");
    let frame = h.frame();
    h.play(RATE / 20);
    assert!(h.frame() > frame);
}

#[test]
fn a_held_engine_lock_plays_silence_counts_misses_and_flags_the_input_gap() {
    let h = Harness::new();
    h.open(asio(Some(256)));
    h.play(RATE / 20);
    {
        let _held = h.host.core.rt.lock().unwrap();
        let callbacks = h.host.diag().callbacks;
        until("callbacks run into the held lock", || h.host.diag().callbacks > callbacks + 3);
    }
    h.play(RATE / 20);
    let diag = h.host.diag();
    assert!(diag.lock_misses >= 3, "{diag:?}");
    assert_eq!(diag.engine.xruns, 1, "the block after the misses follows an input gap");
    assert!(!h.host.core.rt.is_poisoned());
}

#[test]
fn a_late_asio_wake_loses_nothing() {
    let h = Harness::new();
    h.open(asio(Some(256)));
    h.play(RATE / 20);
    h.fake.gap.store(2, SeqCst);
    h.play(RATE / 20);
    let diag = h.host.diag();
    assert_eq!((diag.gaps, diag.engine.xruns), (0, 0), "{diag:?}");
    let run = &h.starts()[0];
    assert!(run.windows(2).all(|w| w[1] - w[0] == 256), "every bufferSwitch is one period on the frame counter");
}

#[test]
fn a_late_wasapi_callback_that_finds_frames_still_queued_loses_nothing() {
    let h = Harness::new();
    h.open(wasapi(None, None));
    h.play(RATE / 20);
    // One period late: the 2.2-period buffer still holds 0.2 of one, and this callback fills two.
    h.fake.gap.store(1, SeqCst);
    h.play(RATE / 20);
    let diag = h.host.diag();
    assert_eq!((diag.gaps, diag.engine.xruns), (0, 0), "{diag:?}");
    let first = h.starts()[0][0];
    assert!(!h.fake.heard(first..h.frame()).iter().any(|s| s.is_nan()), "no frame skipped");
}

#[test]
fn a_wasapi_buffer_that_ran_dry_skips_what_played_dry_in_the_frames_and_the_input_alike() {
    let h = Harness::new();
    // A ramp on the input, so what plays names the input frame it came from.
    const STEP: f32 = 1.0e-7;
    h.fake.set_input(|frame| (frame % (1 << 20)) as f32 * STEP);
    h.open(wasapi(None, None));
    h.send(Command::SetSlotLive(0, true));
    // Input frames behind the frame counter where the input is heard (input 2 plays it doubled).
    let lag = |h: &Harness| {
        let at = h.frame();
        let heard = h.fake.heard(at - 64..at);
        (at - 32) as f64 - heard[31] as f64 / 2.0 / STEP as f64
    };
    h.play(RATE / 2);
    let before = lag(&h);
    // Three periods between wakes (1440 frames) against a 1056-frame buffer: 384 played dry.
    h.fake.gap.store(2, SeqCst);
    h.play(RATE / 10);
    let diag = h.host.diag();
    assert_eq!((diag.gaps, diag.engine.xruns, diag.join_trims), (1, 1, 0), "{diag:?}");
    let first = h.starts()[0][0];
    let skipped = h.fake.heard(first..h.frame()).iter().filter(|s| s.is_nan()).count();
    assert_eq!(skipped, 384, "the frame counter skipped what the device played dry");
    let after = lag(&h);
    assert!((after - before).abs() < 2.0, "the input lands where it did: lag {before:.1} then {after:.1}");
}

#[test]
fn a_cpal_xrun_is_counted_and_flags_the_next_block() {
    let h = Harness::new();
    h.open(asio(Some(256)));
    h.play(RATE / 20);
    h.fake.xrun.store(true, SeqCst);
    h.play(RATE / 20);
    let diag = h.host.diag();
    assert_eq!((diag.xruns, diag.engine.xruns, diag.gaps), (1, 1, 0));
    assert_eq!(h.host.status().map(|s| s.backend), Some(AudioBackend::Asio), "an xrun is not a loss");
}

#[test]
fn an_input_missing_from_its_cycle_is_one_duplex_fault() {
    let h = Harness::new();
    h.open(asio(Some(256)));
    h.play(RATE / 20);
    h.fake.skip_input.store(true, SeqCst);
    h.play(RATE / 10);
    assert_eq!(h.host.diag().duplex_faults, 1, "counted once, then back in step");
}

#[test]
fn an_asio_input_that_starts_cycles_before_its_output_is_no_duplex_fault() {
    let h = Harness::new();
    h.fake.set_input(|_| 0.25);
    h.fake.lead_in.store(3, SeqCst);
    h.open(asio(Some(256)));
    h.send(Command::SetSlotLive(0, true));
    h.play(RATE / 20);
    h.fake.lead_in.store(1, SeqCst);
    h.open(asio(Some(128)));
    h.play(RATE / 20);
    assert_eq!(h.host.diag().duplex_faults, 0, "the output's first callback takes the input's count");
    let at = h.frame();
    assert!(h.fake.heard(at - 64..at).iter().all(|s| (s - 0.5).abs() < 1e-3), "the input is heard");
}

#[test]
fn the_capture_channel_changes_without_a_new_stream() {
    let h = Harness::new();
    h.fake.set_input(|_| 0.25);
    h.open(asio(Some(256)));
    h.send(Command::SetSlotLive(0, true));
    let heard = |h: &Harness| {
        h.play(RATE / 10);
        let at = h.frame();
        h.fake.heard(at - 64..at)
    };
    assert!(heard(&h).iter().all(|s| (s - 0.5).abs() < 1e-3), "auto takes input 2 (0.5 on the fake)");
    h.host.set_input_channel(Some(0)).unwrap();
    assert!(heard(&h).iter().all(|s| (s - 0.25).abs() < 1e-3), "input 1");
    assert!(h.host.set_input_channel(Some(2)).is_err(), "the fake has two inputs");
    let mut same = asio(Some(256));
    same.input_channel = Some(1);
    h.open(same);
    assert!(heard(&h).iter().all(|s| (s - 0.5).abs() < 1e-3), "an open of the running device only changes the channel");
    assert_eq!(h.fake.started.load(SeqCst), 1, "one stream pair throughout");
}

#[test]
fn the_alignment_is_the_drivers_input_plus_output_latency() {
    let h = Harness::new();
    let status = h.open(asio(Some(256)));
    assert_eq!((status.align_frames, status.input_frames), (32 + 48, 32));
    assert_eq!(h.host.status().map(|s| (s.align_frames, s.input_frames)), Some((80, 32)));
}

#[test]
fn share_output_mirrors_the_master_only_while_asio_plays_and_drops_its_taps_off_the_audio_thread() {
    let mut h = Harness::new();
    h.fake.set_input(tone);
    h.host.set_share(Some("obs".into())).unwrap();
    assert!(h.fake.shares.lock().unwrap().is_empty(), "no device yet: only remembered");
    h.open(asio(Some(256)));
    let first = h.fake.shares.lock().unwrap()[0].clone();
    assert_eq!(first.endpoint, "obs");
    h.record_loop();
    until("the mirror is fed", || first.frames.load(SeqCst) > RATE as u64);
    assert!(f32::from_bits(first.peak.load(SeqCst)) > 0.05, "it hears the loop");

    h.host.set_share(Some("discord".into())).unwrap();
    assert_eq!(first.tap_dropped_on.lock().unwrap().as_deref(), Some("lf-engine-owner"));
    let second = h.fake.shares.lock().unwrap()[1].clone();
    until("the new mirror is fed", || second.frames.load(SeqCst) > 0);

    second.faulted.store(true, SeqCst);
    let events = h.device_events(1);
    assert!(matches!(events[0], DeviceEvent::ShareLost { .. }), "{events:?}");
    assert_eq!(second.tap_dropped_on.lock().unwrap().as_deref(), Some("lf-engine-owner"));
    let fed = second.frames.load(SeqCst);
    h.play(RATE / 10);
    assert_eq!(second.frames.load(SeqCst), fed, "the lost mirror is fed no more");

    h.host.set_share(Some("obs".into())).unwrap();
    let third = h.fake.shares.lock().unwrap()[2].clone();
    h.host.close().unwrap();
    assert_eq!(third.tap_dropped_on.lock().unwrap().as_deref(), Some("lf-engine-owner"), "a close takes the mirror down");
    assert_eq!(h.host.diag().share_overruns, 0);
}

#[test]
fn on_wasapi_share_output_opens_no_mirror() {
    let h = Harness::new();
    h.host.set_share(Some("obs".into())).unwrap();
    h.open(wasapi(None, None));
    h.play(RATE / 10);
    assert!(h.fake.shares.lock().unwrap().is_empty(), "app capture takes the main output");
    h.open(asio(Some(256)));
    assert_eq!(h.fake.shares.lock().unwrap().len(), 1, "the remembered pick opens once ASIO plays");
}

#[test]
fn wasapi_joins_the_input_across_a_400_ppm_skew_without_starving() {
    let h = Harness::new();
    h.fake.set_input(|_| 0.25);
    *h.fake.skew_ppm.lock().unwrap() = 400.0;
    let status = h.open(wasapi(None, None));
    assert!(status.input_frames > 32, "the capture's age plus the join's delay: {status:?}");
    h.send(Command::SetSlotLive(0, true));
    h.play(RATE * 20);
    let diag = h.host.diag();
    assert_eq!((diag.join_starves, diag.join_overruns), (0, 0), "{diag:?}");
    let at = h.frame();
    assert!(h.fake.heard(at - 64..at).iter().all(|s| (s - 0.5).abs() < 1e-3), "the input is heard");
}

#[test]
fn a_loop_resumes_in_place_across_a_switch_from_asio_to_wasapi() {
    let mut h = Harness::new();
    h.fake.set_input(tone);
    h.open(asio(Some(256)));
    let length = h.record_loop();
    h.play(2 * length);
    h.open(wasapi(None, None));
    h.play(length / 2);
    assert_resumed_in_place(&h, 1, length);
}

#[test]
fn a_switch_that_fails_to_start_reopens_the_device_it_replaced() {
    let h = Harness::new();
    h.open(asio(Some(256)));
    h.fake.fail_starts.store(1, SeqCst);
    assert!(h.host.open(asio(Some(128))).is_err());
    assert_eq!(h.host.status().map(|s| s.block), Some(256), "the previous device plays again");
    let frame = h.frame();
    h.play(RATE / 20);
    assert!(h.frame() > frame);
    assert!(h.host.open(DeviceRequest { backend: AudioBackend::Wasapi, output: Some("gone".into()), ..asio(None) }).is_err());
    assert_eq!(h.fake.started.load(SeqCst), 2, "a device that does not resolve leaves the running one alone");
}

#[test]
fn a_clean_run_through_switches_and_a_plugin_swap_counts_nothing_and_allocates_nothing() {
    let mut h = Harness::new();
    h.fake.set_input(tone);
    h.open(asio(Some(256)));
    let length = h.record_loop();
    let slot = h.host.slot(0);
    for buffer in [128, 64, 256] {
        let (unit, calls, _) = Unit::new(0.0);
        assert!(slot.install(unit, RATE as u32).is_ok());
        until("the unit renders", || calls.load(SeqCst) > 0);
        h.open(asio(Some(buffer)));
        h.play(length / 4);
        assert!(slot.remove(Duration::from_secs(2)).unwrap().is_some());
    }
    h.host.close().unwrap();
    let diag = h.host.diag();
    assert!(diag.callbacks > 0);
    let quiet = super::IoDiag { callbacks: diag.callbacks, ..Default::default() };
    assert_eq!(diag, quiet, "every counter but the callbacks stays 0");
}

#[test]
fn settings_sent_before_the_first_open_and_across_a_new_rate_reach_every_engine() {
    let mut h = Harness::new();
    let send = |command| h.host.send(TimedCommand { frame: None, command });
    assert!(send(Command::SetBpm(90.0)).is_ok(), "a setting is kept while no engine exists");
    assert!(send(Command::RecDub(0)).is_err(), "an action needs an engine");
    h.open(asio(Some(256)));
    h.wait_event("the first engine starts at the kept tempo", |e| matches!(e, Event::Transport { bpm: 90, .. }).then_some(()));
    h.send(Command::SetBpm(100.0));
    h.wait_event("the tempo changes", |e| matches!(e, Event::Transport { bpm: 100, .. }).then_some(()));
    h.fake.asio.lock().unwrap().as_mut().unwrap().rate = 44_100;
    h.open(asio(Some(128)));
    let bpm = h.wait_event("the new engine reports its transport", |e| match e {
        Event::Transport { bpm, .. } => Some(*bpm),
        _ => None,
    });
    assert_eq!(bpm, 100, "a new engine gets the last tempo sent, before its first publish");
}

/// Tick `feed` until it sends a frame `want` accepts; every frame sent meanwhile goes to `seen`.
fn feed_until(feed: &mut super::feed::Feed, seen: &mut Vec<super::wire::FeedFrame>, what: &str, want: impl Fn(&super::wire::FeedFrame) -> bool) -> super::wire::FeedFrame {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Some(frame) = feed.tick(false) {
            seen.push(frame.clone());
            if want(&frame) {
                return frame;
            }
        }
        assert!(Instant::now() < deadline, "no feed frame: {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn the_feed_resyncs_a_new_subscriber_and_a_new_engine() {
    use super::feed::Feed;
    let h = Harness::new();
    h.fake.set_input(tone);
    let mut feed = Feed::new(h.host.clone());
    let lanes = |f: &super::wire::FeedFrame| f.events.iter().filter(|e| matches!(e.0, Event::Lane { info, .. } if info.state == LaneState::Empty)).count();

    let first = feed.tick(true).expect("a new subscriber gets a frame at once");
    assert!(first.reset && first.status == Some(None) && first.anchor.is_none() && first.meter.is_none());
    assert_eq!(lanes(&first), 5, "every lane, EMPTY");
    assert!(matches!(first.events.last().map(|e| e.0), Some(Event::Selected { lane: 0, .. })));
    assert!(feed.tick(false).is_none(), "no device and nothing changed: nothing to send");

    h.host.send(TimedCommand { frame: None, command: Command::SetBpm(240.0) }).unwrap();
    h.open(asio(Some(256)));
    let mut seen = Vec::new();
    let reset = feed_until(&mut feed, &mut seen, "the first engine resets the UI", |f| f.reset);
    assert!(matches!(reset.status, Some(Some(DeviceStatus { sample_rate: 48_000, .. }))));
    let anchor = feed_until(&mut feed, &mut seen, "the running device's anchor", |f| f.anchor.is_some()).anchor.unwrap();
    assert_eq!(anchor.rate, 48_000);
    assert!(anchor.frame >= 0 && anchor.at_ms > 1.7e12, "a device frame at a Unix time");
    let meter = feed_until(&mut feed, &mut seen, "the input meter moves", |f| f.meter.is_some_and(|m| m.peak > 0.0)).meter.unwrap();
    assert!(meter.peak <= 0.5 && !meter.clip, "the tone's peak, no clip");
    let kept = |f: &super::wire::FeedFrame| f.events.iter().any(|e| matches!(e.0, Event::Transport { bpm: 240, .. }));
    if !seen.iter().any(kept) {
        feed_until(&mut feed, &mut seen, "the kept tempo on the transport", kept);
    }

    // A second subscriber (a WebView reload) gets the whole state again, with the kept settings.
    let again = feed.tick(true).expect("a reset frame");
    assert!(again.reset && matches!(again.status, Some(Some(_))));
    assert_eq!(again.settings.as_deref().map(|s| s.iter().map(|c| c.0).collect::<Vec<_>>()), Some(vec![Command::SetBpm(240.0)]));
    assert!(matches!(again.events.first().map(|e| e.0), Some(Event::Transport { bpm: 240, .. })), "the transport the engine reported");
    assert_eq!(lanes(&again), 5);

    // Another rate builds a new engine: the UI resets again.
    h.fake.asio.lock().unwrap().as_mut().unwrap().rate = 44_100;
    h.open(asio(Some(128)));
    let reset = feed_until(&mut feed, &mut seen, "the new engine resets the UI", |f| f.reset);
    assert!(reset.seq > again.seq);
    let anchor = feed_until(&mut feed, &mut seen, "the new device's anchor", |f| f.anchor.is_some_and(|a| a.rate == 44_100)).anchor.unwrap();
    assert_eq!(anchor.rate, 44_100);
    h.host.close().unwrap();
    let closed = feed_until(&mut feed, &mut seen, "the stopped device", |f| f.status == Some(None));
    assert!(closed.anchor.is_none() && closed.meter.is_none());
    assert!(seen.windows(2).all(|w| w[1].seq > w[0].seq), "frames in order");
}

#[test]
fn the_feed_draws_a_take_as_it_records_and_the_whole_loop_on_a_reset() {
    use super::feed::Feed;
    use super::wire::FeedFrame;
    let h = Harness::new();
    h.fake.set_input(tone);
    let mut feed = Feed::new(h.host.clone());
    let mut seen = Vec::new();
    let lane0 = |f: &FeedFrame, want: fn(&LaneInfo) -> bool| f.events.iter().any(|e| matches!(e.0, Event::Lane { lane: 0, info, .. } if want(&info)));
    h.open(asio(Some(256)));
    for command in [Command::SetBpm(240.0), Command::SetSlotLive(0, true), Command::RecDub(0)] {
        h.send(command);
    }
    feed_until(&mut feed, &mut seen, "the take starts", |f| lane0(f, |i| i.state == LaneState::Recording && !i.armed));
    let growing = feed_until(&mut feed, &mut seen, "the take draws as it records", |f| f.peaks.iter().any(|p| p.lane == 0 && p.count >= 4));
    let update = growing.peaks.iter().find(|p| p.lane == 0).unwrap();
    // The fake's input 2 carries the tone at 0.2.
    assert!(update.max.iter().all(|&m| (0.0..=0.201).contains(&m)) && update.min.iter().all(|&m| (-0.201..=0.0).contains(&m)), "the tone's peaks: {update:?}");
    h.play(RATE * 3 / 2);
    h.send(Command::RecDub(0));
    let playing = feed_until(&mut feed, &mut seen, "the loop plays", |f| lane0(f, |i| i.state == LaneState::Playing && i.length > 0));
    let length = playing.events.iter().find_map(|e| match e.0 {
        Event::Lane { lane: 0, info, .. } if info.length > 0 => Some(info.length),
        _ => None,
    });
    let bins = (length.unwrap() as usize).div_ceil(lf_engine::overview::PEAK_FRAMES) as u32;

    let reset = feed.tick(true).expect("a reset frame");
    let full: Vec<_> = reset.peaks.iter().filter(|p| p.lane == 0).collect();
    assert_eq!(full.len(), 1, "the whole loop in one run");
    assert_eq!((full[0].start, full[0].count, full[0].min.len() as u32), (0, bins, bins));
    assert!(full[0].max.iter().filter(|&&m| m > 0.19).count() as u32 > bins / 2, "the loop holds the tone");
    assert!(reset.peaks.iter().all(|p| p.lane == 0), "the empty lanes send nothing");

    h.send(Command::Clear(0));
    let cleared = feed_until(&mut feed, &mut seen, "the cleared lane's empty update", |f| f.peaks.iter().any(|p| p.lane == 0 && p.count == 0));
    assert!(cleared.peaks.iter().any(|p| p.lane == 0 && p.min.is_empty()));
}

#[test]
fn a_wasapi_endpoint_with_no_input_plays_output_only() {
    let h = Harness::new();
    h.fake.wasapi.lock().unwrap()[0].1.in_channels = 0;
    let status = h.host.open(DeviceRequest { input_channel: Some(1), ..wasapi(None, None) }).expect("output only opens");
    assert!(!status.input_open && status.input_name.is_empty(), "{status:?}");
    let frame = h.frame();
    h.play(RATE / 10);
    assert!(h.frame() > frame, "the device plays");
    assert!(h.host.set_input_channel(Some(0)).is_ok(), "a channel pick changes nothing and is no error");
    let (peak, _) = h.host.take_meter();
    assert_eq!(peak, 0.0, "the input is silence");
    assert_eq!(h.host.diag().join_starves, 0, "a join that never had input is no starve");
    h.fake.wasapi.lock().unwrap()[0].1.in_channels = 2;
    assert!(h.open(asio(Some(256))).input_open);
}

#[test]
fn a_lane_the_engine_clears_forgets_its_kept_mix() {
    use super::feed::Feed;
    let h = Harness::new();
    let mut feed = Feed::new(h.host.clone());
    h.open(asio(Some(256)));
    for command in [Command::SetVolume(3, 0.5), Command::SetMute(3, true), Command::SetMasterVolume(0.7), Command::ClearAll] {
        h.send(command);
    }
    let mut seen = Vec::new();
    feed_until(&mut feed, &mut seen, "the engine clears every lane", |f| f.events.iter().any(|e| matches!(e.0, Event::Cleared { lane: 3, .. })));
    let kept = feed.tick(true).and_then(|f| f.settings).map(|s| s.into_iter().map(|c| c.0).collect::<Vec<_>>());
    assert_eq!(kept, Some(vec![Command::SetMasterVolume(0.7)]), "lane 3's volume and mute went with the clear");
}

/// A snapshot's bytes: its header as JSON and its PCM.
fn session_parts(bytes: &[u8]) -> (serde_json::Value, Vec<f32>) {
    let len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let header = serde_json::from_slice(&bytes[4..4 + len]).expect("a JSON header");
    let pcm = bytes[4 + len..].chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
    (header, pcm)
}

fn session_bytes(header: &serde_json::Value, pcm: &[f32]) -> Vec<u8> {
    let json = serde_json::to_vec(header).unwrap();
    let mut out = (json.len() as u32).to_le_bytes().to_vec();
    out.extend_from_slice(&json);
    pcm.iter().for_each(|x| out.extend_from_slice(&x.to_le_bytes()));
    out
}

#[test]
fn a_session_saves_and_loads_back_through_its_bytes_with_a_device_running_or_not() {
    let mut h = Harness::new();
    h.fake.set_input(tone);
    h.open(asio(Some(256)));
    let length = h.record_loop();
    h.send(Command::Reverse(0));
    h.play(RATE / 10);
    let (header, pcm) = session_parts(&h.host.snapshot().expect("a snapshot while the device plays"));
    assert_eq!(header["rate"], 48_000);
    assert_eq!(header["bpm"], 240);
    assert_eq!(header["masterLengthFrames"], length);
    assert_eq!(header["tracks"], serde_json::json!([{ "index": 0, "frames": length, "reversed": true, "state": "Playing" }]));
    assert_eq!(pcm.len(), length as usize);
    assert!(pcm.iter().any(|&x| x.abs() > 0.1), "the loop holds the tone");

    let load = serde_json::json!({ "bpm": 240, "bars": 1, "masterLengthFrames": length, "tracks": header["tracks"] });
    let err = h.host.load_session(&session_bytes(&load, &pcm)).expect_err("the lanes are not empty");
    assert!(err.contains("not empty"), "{err}");
    h.send(Command::ClearAll);
    h.wait_lane("the lane clears", 0, |i| i.state == LaneState::Empty);
    h.host.load_session(&session_bytes(&load, &pcm)).expect("a load into the emptied engine");
    let (again, back) = session_parts(&h.host.snapshot().unwrap());
    assert_eq!(again["tracks"], header["tracks"]);
    assert_eq!(back, pcm, "the loop comes back sample-exact, reversed flag and all");

    h.host.close().unwrap();
    let (closed, idle) = session_parts(&h.host.snapshot().expect("a snapshot with no device running"));
    assert_eq!((closed["tracks"].clone(), idle), (header["tracks"].clone(), pcm.clone()));
    let wrong = serde_json::json!({ "bpm": 120, "bars": 1, "masterLengthFrames": length, "tracks": header["tracks"] });
    assert!(h.host.load_session(&session_bytes(&wrong, &pcm)).is_err(), "a tempo that disagrees with the length");
    let diag = h.host.diag();
    assert_eq!((diag.rt_allocs, diag.lock_misses, diag.panics), (0, 0, 0), "the engine side allocated nothing and the callback never missed");
}
