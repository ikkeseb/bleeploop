//! The device owner and the callbacks on the fake driver (`fake_driver.rs`): no hardware, the real
//! engine. Each test drives an [`EngineHost`] as its callers will (open, switch, close, the slot
//! handshake) and reads what the fake played, the counters and the device events.

use std::any::Any;
use std::sync::atomic::{AtomicUsize, Ordering::{Relaxed, SeqCst}};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use lf_engine::grid::Frame;
use lf_engine::{Command, Event, LaneInfo, LaneState, SlotEvent, SlotKind, SlotProcessor, TimedCommand};

use super::callback::Side;
use super::fake_driver::{Fake, FakeDevice, FakeDriver};
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
/// call numbered `panic_at`, if one is set.
struct Unit {
    level: f32,
    calls: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
    panic_at: Option<usize>,
}

impl Unit {
    fn new(level: f32) -> (Box<Unit>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let (calls, stops) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        (Box::new(Unit { level, calls: calls.clone(), stops: stops.clone(), panic_at: None }), calls, stops)
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
    assert!(slot.install(unit).is_ok());
    until("the unit renders", || calls.load(SeqCst) > 0);

    h.fake.asio.lock().unwrap().as_mut().unwrap().rate = 44_100;
    let status = h.open(asio(Some(128)));
    assert_eq!(status.sample_rate, 44_100);
    assert_eq!(slot.rate(), Some(44_100), "what the owner re-activates the unit at");
    let back = slot.take_evicted().expect("the evicted unit waits for its owner");
    assert_eq!(stops.load(SeqCst), 1, "stopped once, before it left");
    let rendered = calls.load(SeqCst);
    assert!(slot.install(back).is_ok(), "the new engine takes it");
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
        assert!(slot.install(unit).is_ok(), "round {round}");
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
fn a_panicking_unit_counts_one_panic_goes_silent_and_leaves_the_lock_unpoisoned() {
    let h = Harness::new();
    h.open(asio(Some(256)));
    let (mut unit, calls, _) = Unit::new(0.5);
    unit.panic_at = Some(50);
    h.send(Command::SetSlotLive(0, true));
    assert!(h.host.slot(0).install(unit).is_ok());
    until("the unit panics", || h.host.diag().panics == 1);
    let at = h.frame();
    h.play(RATE / 10);
    assert!(h.fake.heard(at..at + RATE / 20).iter().all(|&s| s == 0.0), "silence after the panic");
    assert_eq!(calls.load(SeqCst), 50, "the engine is not called again");
    assert_eq!(h.host.diag().panics, 1);
    #[cfg(debug_assertions)]
    assert!(h.host.diag().rt_allocs > 0, "the guard counts: the panic's payload allocated under it");
    assert!(!h.host.core.rt.is_poisoned());
    h.host.close().unwrap();
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
fn a_late_callback_is_a_gap_that_jumps_the_frame_counter_and_flags_an_xrun() {
    let h = Harness::new();
    h.open(asio(Some(256)));
    h.play(RATE / 20);
    h.fake.gap.store(2, SeqCst);
    h.play(RATE / 20);
    let diag = h.host.diag();
    assert_eq!((diag.gaps, diag.engine.xruns), (1, 1));
    let run = &h.starts()[0];
    let jumps: Vec<Frame> = run.windows(2).map(|w| w[1] - w[0]).filter(|&d| d != 256).collect();
    assert_eq!(jumps, vec![3 * 256], "the two periods the device lost");
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
        assert!(slot.install(unit).is_ok());
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
