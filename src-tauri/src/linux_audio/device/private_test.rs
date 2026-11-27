//! Driven by verify/linux-app/private-graph.py on a new server with two virtual stereo endpoints.
use super::*;
use lf_engine::{Command, EngineConfig, TimedCommand};

// Advertise a known playback latency from a JACK node; null PipeWire adapters have zero latency
// and do not implement ProcessLatency. The owning client closes before its callback state drops.
struct FixtureLatency {
    port: *mut Port,
    frames: AtomicU32,
}
struct Sink {
    client: Client,
    latency: Box<FixtureLatency>,
}
extern "C" fn fixture_latency(mode: c_uint, arg: *mut c_void) {
    if mode == 1 {
        let state = unsafe { &*arg.cast::<FixtureLatency>() };
        let frames = state.frames.load(Acquire);
        let mut range = jack::Latency {
            min: frames,
            max: frames,
        };
        unsafe {
            jack::jack_port_set_latency_range(state.port, mode, &mut range);
        }
    }
}
extern "C" fn discard(_: u32, _: *mut c_void) -> c_int {
    0
}
impl Sink {
    fn new() -> Self {
        let client = Client::open(c"LatencyFixture").unwrap();
        let port = client.register(c"playback_M", INPUT).unwrap();
        let sink = Self {
            client,
            latency: Box::new(FixtureLatency {
                port,
                frames: 37.into(),
            }),
        };
        unsafe {
            let raw = sink.client.raw();
            check(
                "fixture process",
                jack::jack_set_process_callback(raw, discard, std::ptr::null_mut()),
            )
            .unwrap();
            check(
                "fixture latency",
                jack::jack_set_latency_callback(
                    raw,
                    fixture_latency,
                    (&*sink.latency as *const FixtureLatency).cast_mut().cast(),
                ),
            )
            .unwrap();
            check("fixture activate", jack::jack_activate(raw)).unwrap();
        }
        sink
    }
    fn publish(&self, frames: u32) {
        self.latency.frames.store(frames, Release);
        unsafe {
            jack::jack_recompute_total_latencies(self.client.raw());
        }
    }
}

#[test]
#[ignore = "run with verify/linux-app/private-graph.py on its isolated PipeWire server"]
fn private_jack_quantum_and_routes() {
    let dir = std::env::var_os("BLEEPLOOP_TEST_GRAPH").expect("private graph runner required");
    assert_eq!(std::env::var_os("PIPEWIRE_RUNTIME_DIR"), Some(dir));
    assert_eq!(
        std::env::var("PIPEWIRE_REMOTE").as_deref(),
        Ok("pipewire-0")
    );
    let sink = Sink::new();
    let inputs = list_inputs().unwrap();
    assert!(inputs
        .iter()
        .any(|e| e.id == "fixture_in:monitor" && e.channels == 2));
    let server = DeviceRequest {
        backend: "Jack".into(),
        input: Some("fixture_in:monitor".into()),
        output: None,
        input_channels: [Some(InputPick::Pair([0, 1])), Some(InputPick::Mono(7))],
        buffer: None,
        sample_rate: None,
    };
    let mut first = Prepared::new(&server).unwrap();
    let rate = first.rate();
    first.request_buffer(None).unwrap();
    first.settle().unwrap();
    let mut config = EngineConfig::new(rate);
    config.max_loop_seconds = 1.0;
    let (engine, mut handle) = Engine::new(config);
    let mut device = first.start(Box::new(engine), 0).map_err(|e| e.0).unwrap();
    let original = device.status().block;
    assert_eq!(device.status().input_channels[1], InputPick::Mono(1));
    device.set_channel(0, Some(InputPick::Mono(0))).unwrap();
    assert_eq!(device.status().input_requests[1], Some(InputPick::Mono(7)));
    assert!(device.set_channel(0, Some(InputPick::Mono(3))).is_err());
    device.set_channel(0, server.input_channels[0]).unwrap();

    let pick = DeviceRequest {
        output: Some(device.status().output_name),
        ..server.clone()
    };
    assert!(
        device.can_keep(&server, &pick),
        "explicit running endpoint keeps the stream"
    );
    let pinned = DeviceRequest {
        buffer: Some(original),
        ..pick.clone()
    };
    assert!(
        !device.can_keep(&pick, &pinned),
        "an explicit size must acquire its own preference"
    );
    let switch = |device: Device, req: &DeviceRequest| {
        let mut next = Prepared::new(req).unwrap();
        next.request_buffer(req.buffer).unwrap();
        let (engine, frame, fault) = device.stop();
        assert!(!fault);
        next.settle().unwrap();
        next.start(engine, frame).map_err(|e| e.0).unwrap()
    };
    let device = switch(device, &pinned);
    assert_eq!(device.status().block, original);
    let small = if original == 128 { 64 } else { 128 };
    let low = DeviceRequest {
        buffer: Some(small),
        output: Some("LatencyFixture".into()),
        ..pick.clone()
    };
    let device = switch(device, &low);
    assert_eq!(device.status().block, small);
    let initial_latency = device.status().align_frames - device.status().input_frames;
    assert_eq!(initial_latency, 37, "advertised playback latency at open");
    sink.publish(137);
    wait_ready(
        &device.run,
        || device.status().align_frames - device.status().input_frames == 137,
        "Changed fixture latency did not reach DeviceStatus",
    )
    .unwrap();
    println!("Runtime JACK playback latency {initial_latency} → 137 frames");
    let high = DeviceRequest {
        buffer: Some(small * 2),
        ..low.clone()
    };
    let device = switch(device, &high);
    assert_eq!(
        device.status().block,
        small * 2,
        "increase from a still-active smaller request"
    );
    let missing = DeviceRequest {
        input: Some("missing-fixture".into()),
        ..high.clone()
    };
    assert!(matches!(Prepared::new(&missing),Err(e) if e.contains("unavailable")));
    let before = device.anchor().map_or(0, |a| a.0);
    handle
        .commands
        .push(TimedCommand {
            frame: None,
            command: Command::SetSlotLive(0, true),
        })
        .unwrap();
    wait_ready(
        &device.run,
        || handle.overview.slot_live(0) && device.anchor().is_some_and(|a| a.0 > before),
        "failed switch stopped the device",
    )
    .unwrap();
    assert_eq!(device.fault(), None);

    let changed = DeviceRequest {
        output: Some("fixture_in".into()),
        input: None,
        ..high.clone()
    };
    let device = switch(device, &changed);
    assert_eq!(
        device.status().block,
        small * 2,
        "port change must retain explicit quantum"
    );
    let (engine, frame, fault) = device.stop();
    assert!(!fault);
    let mut next = Prepared::new(&high).unwrap();
    next.request_buffer(high.buffer).unwrap();
    next.settle().unwrap();
    let device = next.start(engine, frame).map_err(|e| e.0).unwrap();
    assert_eq!(device.status().block, small * 2);
    assert_eq!(device.status().input_name, "fixture_in (monitor)");
    assert_eq!(device.status().output_name, "LatencyFixture");
    let follow = DeviceRequest {
        buffer: None,
        ..high.clone()
    };
    assert!(!device.can_keep(&high, &follow));
    let device = switch(device, &follow);
    assert_eq!(
        device.status().block,
        original,
        "return the actual default after the old node closes"
    );
    assert!(handle.overview.slot_live(0));
    println!("JACK {rate} Hz: pin {original}, decrease to {small}, increase to {}, change port, reopen, follow server at {original}; refused switch retained clock and command", small*2);
}

#[test]
fn a_graph_report_rejects_an_input_take_but_preserves_contiguous_generated_audio() {
    use lf_engine::{Event, Instrument, NoteTarget};
    const REPORT: i64 = 12_032; // inside the fixed take, aligned to the 128-frame device period
    let cases: &[(&str, bool, bool, &[(i64, bool)], bool)] = &[
        ("synth only", false, true, &[], false),
        ("local frame gap while playing synth", true, true, &[], true),
        ("external input live", false, true, &[(0, true)], true),
        ("no external port", false, false, &[(0, true)], false),
        (
            "live opens on report frame",
            false,
            true,
            &[(REPORT, true)],
            true,
        ),
        (
            "live changes in second engine slice",
            false,
            true,
            &[(REPORT + 80, true)],
            true,
        ),
        (
            "ramp finished inside previous period",
            false,
            true,
            &[(0, true), (REPORT - 96, false)],
            true,
        ),
        (
            "ramp was already closed two periods ago",
            false,
            true,
            &[(0, true), (REPORT - 384, false)],
            false,
        ),
    ];
    for &(name, clock_gap, has_capture, live, reject) in cases {
        let mut config = EngineConfig::new(8_000);
        config.max_loop_seconds = 2.0;
        let (mut engine, mut handle) = Engine::new(config);
        let mut queue = |frame, command| {
            handle
                .commands
                .push(TimedCommand {
                    frame: Some(frame),
                    command,
                })
                .unwrap()
        };
        for (t, c) in [
            (0, Command::SetBpm(240.0)),
            (0, Command::SetFixedBars(1.0)),
            (0, Command::SetFixedLength(true)),
            (
                0,
                Command::SelectInstrument(NoteTarget::Builtin(Instrument::Organ)),
            ),
            (0, Command::RecDub(0)),
            (8_512, Command::NoteOn(60, 0.25)),
            (15_000, Command::NoteOff(60)),
        ] {
            queue(t, c);
        }
        for &(t, on) in live {
            queue(t, Command::SetSlotLive(1, on));
        }
        let mut previous = false;
        let mut rejected = false;
        let mut frame = 0;
        let input = [0.0; 64];
        let (mut left, mut right) = ([0.0; 64], [0.0; 64]);
        while frame < 20_000 {
            let report = frame == REPORT;
            let (missed, mut activity) =
                period_gap(report && clock_gap, report, has_capture, previous);
            if report && clock_gap {
                frame += 128;
            }
            for offset in [0, 64] {
                let ctx = ProcessContext {
                    frame: frame + offset,
                    xrun: missed && offset == 0,
                    damaged: false,
                    align_frames: 0,
                    input_frames: 0,
                };
                engine.process_inputs_observed(
                    &ctx,
                    [[&input, &input]; SLOT_COUNT],
                    &mut left,
                    &mut right,
                    &mut activity,
                );
            }
            previous = activity.routed;
            while let Ok(e) = handle.events.pop() {
                rejected |= matches!(e, Event::TakeRejected { .. });
            }
            frame += 128;
        }
        assert_eq!(rejected, reject, "{name}");
        assert_eq!(
            engine.looper().master(),
            if reject { 0 } else { 8_000 },
            "{name}"
        );
    }
}
