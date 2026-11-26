//! Exercise the real engine in a duplex JACK callback on an existing Linux audio graph.
mod jack;

use lf_engine::grid::{frames_per_bar, Frame, Meter};
use lf_engine::{
    Command, Engine, EngineConfig, EngineHandle, Event, Instrument, LaneState, NoteTarget,
    ProcessContext, TimedCommand,
};
use rtrb::{Producer, RingBuffer};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const BPM: f64 = 240.0;
const MAX_BLOCK: usize = 8192;
const DEVICE_GAIN: f32 = 0.05; // Hard ceiling: about -26 dBFS independent of engine settings.
                               // Allow small differences in the master processing of live and looped audio.
const MAX_REPLAY_RELATIVE_RMS: f64 = 0.05;
const SIGNAL_FLOOR: f32 = 1.0e-5;
const ROUTE_WARMUP: Duration = Duration::from_millis(500);

#[derive(Clone, Copy)]
struct Sequence {
    rate: Frame,
    bar: Frame,
    arm: Frame,
    record: Frame,
    playback: Frame,
    stop: Frame,
    silent: Frame,
    end: Frame,
}

impl Sequence {
    fn new(rate: u32) -> Self {
        let bar = frames_per_bar(BPM, rate, Meter::FOUR_FOUR);
        let rate = Frame::from(rate);
        let arm = rate / 4;
        let record = arm + bar;
        let playback = record + bar + rate / 8;
        let stop = playback + 2 * bar;
        let silent = stop + rate / 4;
        Self {
            rate,
            bar,
            arm,
            record,
            playback,
            stop,
            silent,
            end: silent + rate / 4,
        }
    }

    fn queue(self, handle: &mut EngineHandle) -> Result<(), String> {
        use Command::*;
        for (frame, command) in [
            (0, SetBpm(BPM)),
            (0, SetMetronome(false)),
            (0, SetClickVolume(0.0)),
            (0, SetSlotLive(0, false)),
            (0, SetSlotLive(1, false)),
            (0, SetFixedBars(1.0)),
            (0, SetFixedLength(true)),
            (0, SelectInstrument(NoteTarget::Builtin(Instrument::Organ))),
            (self.arm, RecDub(0)),
            (self.record + self.rate / 8, NoteOn(60, 0.5)),
            (self.record + self.rate / 3, NoteOff(60)),
            (self.record + self.rate / 2, NoteOn(67, 0.5)),
            (self.record + self.rate * 5 / 8, NoteOff(67)),
            (self.stop, Stop(0)),
        ] {
            handle
                .commands
                .push(TimedCommand {
                    frame: Some(frame),
                    command,
                })
                .map_err(|_| "Engine command ring filled while queuing the probe".to_owned())?;
        }
        Ok(())
    }
}

#[derive(Default)]
struct Window {
    frames: Frame,
    peak: [f32; 2],
    signal: [Frame; 2],
}

impl Window {
    fn sample(&mut self, samples: [f32; 2]) {
        self.frames += 1;
        for (channel, sample) in samples.into_iter().enumerate() {
            let peak = sample.abs();
            self.peak[channel] = self.peak[channel].max(peak);
            self.signal[channel] += Frame::from(peak >= SIGNAL_FLOOR);
        }
    }
}

#[derive(Default)]
struct ReplayError {
    reference_energy: [f64; 2],
    error_energy: [f64; 2],
}

impl ReplayError {
    fn sample(&mut self, reference: [f32; 2], output: [f32; 2]) {
        for ch in 0..2 {
            let reference = f64::from(reference[ch]);
            let error = f64::from(output[ch]) - reference;
            self.reference_energy[ch] += reference * reference;
            self.error_energy[ch] += error * error;
        }
    }

    fn relative_rms(&self) -> [f64; 2] {
        std::array::from_fn(|ch| (self.error_energy[ch] / self.reference_energy[ch]).sqrt())
    }
}

#[derive(Default)]
struct Report {
    callbacks: usize,
    min_block: usize,
    max_block: usize,
    frames: Frame,
    input_peak: f32,
    idle: Window,
    live: Window,
    loops: [Window; 2],
    replay_error: [ReplayError; 2],
    stopped: Window,
}

enum Fault {
    ClockGap {
        expected: u32,
        actual: u32,
        quantum: usize,
    },
    BlockSize(usize),
    InvalidSample,
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClockGap { expected, actual, quantum } => write!(
                f, "JACK device frame clock skipped or repeated a block: expected={expected} actual={actual} quantum={quantum}",
            ),
            Self::BlockSize(quantum) => write!(f, "Unsupported JACK block size: quantum={quantum}"),
            Self::InvalidSample => f.write_str("JACK input or engine output contained a non-finite sample"),
        }
    }
}

struct Callback {
    engine: Engine,
    seq: Sequence,
    live_bar: Vec<[f32; 2]>,
    live_start: Frame,
    armed: Arc<AtomicBool>,
    result: Producer<Result<Report, Fault>>,
    report: Report,
    origin: Option<u32>,
    finished: bool,
}

impl Callback {
    fn process(&mut self, device_frame: u32, input: &[f32], left: &mut [f32], right: &mut [f32]) {
        if self.finished || !self.armed.load(Ordering::Acquire) {
            left.fill(0.0);
            right.fill(0.0);
            return;
        }

        let origin = *self.origin.get_or_insert(device_frame);
        let frame = Frame::from(device_frame.wrapping_sub(origin));
        let frames = input.len();
        let fault = if frame != self.report.frames {
            Some(Fault::ClockGap {
                expected: origin.wrapping_add(self.report.frames as u32),
                actual: device_frame,
                quantum: frames,
            })
        } else if !(1..=MAX_BLOCK).contains(&frames) {
            Some(Fault::BlockSize(frames))
        } else {
            None
        };
        if let Some(fault) = fault {
            left.fill(0.0);
            right.fill(0.0);
            self.finished = true;
            let _ = self.result.push(Err(fault));
            return;
        }

        let render = frames.min((self.seq.end - frame) as usize);
        self.engine.process(
            &ProcessContext {
                frame,
                xrun: false,
                damaged: false,
                align_frames: 0,
                input_frames: 0,
            },
            &input[..render],
            &mut left[..render],
            &mut right[..render],
        );
        left[render..].fill(0.0);
        right[render..].fill(0.0);

        let mut non_finite = false;
        for i in 0..render {
            let [l, r] = [left[i], right[i]].map(|sample| {
                if sample.is_finite() {
                    sample.clamp(-1.0, 1.0) * DEVICE_GAIN
                } else {
                    non_finite = true;
                    0.0
                }
            });
            left[i] = l;
            right[i] = r;
            non_finite |= !input[i].is_finite();
            self.report.input_peak = self.report.input_peak.max(input[i].abs());

            let at = frame + i as Frame;
            let window = if at < self.seq.arm {
                &mut self.report.idle
            } else if at >= self.live_start && at < self.live_start + self.seq.bar {
                self.live_bar[(at - self.live_start) as usize] = [l, r];
                &mut self.report.live
            } else if at >= self.seq.playback && at < self.seq.stop {
                let cycle = (at - self.seq.playback) / self.seq.bar;
                let offset = (at - self.seq.record) % self.seq.bar;
                self.report.replay_error[cycle as usize]
                    .sample(self.live_bar[offset as usize], [l, r]);
                &mut self.report.loops[cycle as usize]
            } else if at >= self.seq.silent {
                &mut self.report.stopped
            } else {
                continue;
            };
            window.sample([l, r]);
        }
        self.report.callbacks += 1;
        self.report.min_block = self.report.min_block.min(frames);
        self.report.max_block = self.report.max_block.max(frames);
        self.report.frames += render as Frame;
        if non_finite || self.report.frames == self.seq.end {
            self.finished = true;
            let result = if non_finite {
                Err(Fault::InvalidSample)
            } else {
                Ok(std::mem::take(&mut self.report))
            };
            // This single report always has space in its private ring.
            let _ = self.result.push(result);
        }
    }
}

#[derive(Default)]
struct Lane {
    recording: Option<Frame>,
    playing: Option<(Frame, Frame)>,
    stopped: Option<Frame>,
}

impl Lane {
    fn observe(&mut self, handle: &mut EngineHandle) -> Result<(), String> {
        while let Ok(event) = handle.events.pop() {
            match event {
                Event::Lane {
                    frame,
                    lane: 0,
                    info,
                } => match info.state {
                    LaneState::Recording if !info.armed => {
                        self.recording.get_or_insert(frame);
                    }
                    LaneState::Playing => {
                        self.playing.get_or_insert((frame, info.length));
                    }
                    LaneState::Stopped => {
                        self.stopped.get_or_insert(frame);
                    }
                    _ => {}
                },
                Event::Refused { .. } | Event::TakeRejected { .. } => {
                    return Err(format!("Engine refused the probe's sequence: {event:?}"));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

fn run() -> Result<(), String> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let [capture, left, right] = args.as_slice() else {
        return Err(
            "Usage: bleeploop-linux-audio-probe CAPTURE_PORT PLAYBACK_LEFT PLAYBACK_RIGHT".into(),
        );
    };

    let device = jack::Client::open()?;
    let events = Arc::clone(device.events());
    let result = run_sequence(device, capture, left, right);
    // JACK can retire the client before a link query returns. After close, its first callback
    // fault takes precedence over any secondary connection or timeout error.
    events.check_buffers()?;
    result
}

fn run_sequence(
    mut device: jack::Client,
    capture: &str,
    left: &str,
    right: &str,
) -> Result<(), String> {
    let capture = device.endpoint(capture, jack::OUTPUT)?;
    let left = device.endpoint(left, jack::INPUT)?;
    let right = device.endpoint(right, jack::INPUT)?;
    if left.uuid == right.uuid {
        return Err("Choose two distinct JACK playback ports".into());
    }
    let rate = device.sample_rate();
    let initial_block = device.buffer_size();
    if !(8000..=192_000).contains(&rate) || !(1..=MAX_BLOCK as u32).contains(&initial_block) {
        return Err(format!(
            "Unsupported JACK stream: {rate} Hz, {initial_block} frames"
        ));
    }
    println!("JACK sample_rate={rate} initial_block={initial_block} gain={DEVICE_GAIN}");
    for (label, port) in [
        ("capture", &capture),
        ("playback_left", &left),
        ("playback_right", &right),
    ] {
        println!("{label}={:?} physical={}", port.name, port.physical);
    }

    let seq = Sequence::new(rate);
    let mut config = EngineConfig::new(rate);
    config.max_loop_seconds = 2.0;
    config.max_block = MAX_BLOCK;
    let (engine, mut handle) = Engine::new(config);
    seq.queue(&mut handle)?;

    let armed = Arc::new(AtomicBool::new(false));
    let (producer, mut report) = RingBuffer::new(1);
    let mut callback = Callback {
        live_bar: vec![[0.0; 2]; seq.bar as usize],
        // The live output carries the limiter delay that the looper compensates when recording.
        live_start: seq.record + engine.limiter_latency(),
        engine,
        seq,
        armed: Arc::clone(&armed),
        result: producer,
        report: Report {
            min_block: usize::MAX,
            ..Report::default()
        },
        origin: None,
        finished: false,
    };
    let [input, out_l, out_r] = device.activate(move |frame, input, left, right| {
        callback.process(frame, input, left, right);
    })?;
    println!("JACK client active; connecting capture and playback");
    device.connect(&capture.name, &input)?;
    device.connect(&out_l, &left.name)?;
    device.connect(&out_r, &right.name)?;
    let has_routes = || {
        device.exclusive_route(&input, &capture.name)
            && device.exclusive_route(&out_l, &left.name)
            && device.exclusive_route(&out_r, &right.name)
    };
    let route_deadline = Instant::now() + Duration::from_secs(1);
    while !has_routes() {
        device.events().check_buffers()?;
        if Instant::now() >= route_deadline {
            return Err("The requested JACK capture/playback links did not become active".into());
        }
        thread::sleep(Duration::from_millis(10));
    }

    println!(
        "JACK duplex routes active; warming up the graph for {} ms",
        ROUTE_WARMUP.as_millis()
    );
    thread::sleep(ROUTE_WARMUP);
    device.events().check_buffers()?;
    if !has_routes() {
        return Err("A requested JACK route changed during warm-up".into());
    }
    println!("JACK graph warm-up complete; starting the sequence");
    let events = device.events();
    let start_xruns = events.xruns.load(Ordering::Relaxed);
    let mut lane = Lane::default();
    let start = Instant::now();
    armed.store(true, Ordering::Release);
    let report = loop {
        events.check_buffers()?;
        lane.observe(&mut handle)?;
        if !has_routes() {
            return Err("A requested JACK route changed during the probe".into());
        }
        if events.sample_rate.load(Ordering::Relaxed) != rate
            || events.freewheel.load(Ordering::Relaxed)
        {
            return Err("JACK changed sample rate or entered freewheel mode".into());
        }
        if let Ok(report) = report.pop() {
            break report.map_err(|fault| fault.to_string())?;
        }
        if start.elapsed() >= Duration::from_secs(8) {
            return Err("Timed out waiting for real JACK playback callbacks".into());
        }
        thread::sleep(Duration::from_millis(15));
    };
    let elapsed = start.elapsed().as_secs_f64();
    let xruns = events
        .xruns
        .load(Ordering::Relaxed)
        .wrapping_sub(start_xruns);
    lane.observe(&mut handle)?;
    // Close this client and remove its links before printing and evaluating the final results.
    drop(device);

    println!(
        "callbacks={} quantum={}..{} frames={} wall_seconds={elapsed:.3} xruns={xruns} capture_peak={:.7}",
        report.callbacks, report.min_block, report.max_block, report.frames, report.input_peak,
    );
    println!(
        "lane recording={:?} playing={:?} stopped={:?}",
        lane.recording, lane.playing, lane.stopped,
    );
    for (label, window) in [
        ("idle", &report.idle),
        ("live", &report.live),
        ("loop_1", &report.loops[0]),
        ("loop_2", &report.loops[1]),
        ("after_stop", &report.stopped),
    ] {
        println!(
            "{label} frames={} signal_frames={:?} peak={:.7}/{:.7}",
            window.frames, window.signal, window.peak[0], window.peak[1]
        );
    }

    if xruns != 0 || elapsed < 0.8 * report.frames as f64 / f64::from(rate) {
        return Err("JACK did not deliver an uninterrupted real-time device run".into());
    }
    let expected_timeline = lane
        .recording
        .is_some_and(|frame| frame >= seq.record && frame < seq.playback)
        && lane
            .playing
            .is_some_and(|(frame, length)| frame < seq.playback && length == seq.bar)
        && lane.stopped.is_some_and(|frame| frame >= seq.stop);
    if !expected_timeline {
        return Err("Expected one fixed-bar take to record, play and stop".into());
    }
    if report
        .live
        .signal
        .iter()
        .any(|&frames| frames < seq.rate / 10)
        || report.loops.iter().any(|window| {
            window.frames != seq.bar || window.signal.iter().any(|&frames| frames < seq.rate / 10)
        })
    {
        return Err(
            "Expected both JACK outputs to carry the live synth and two loop cycles".into(),
        );
    }
    for (i, error) in report.replay_error.iter().enumerate() {
        let rms = error.relative_rms();
        println!("loop_{} relative_rms_error={rms:.5?}", i + 1);
        if rms
            .iter()
            .any(|&value| !value.is_finite() || value > MAX_REPLAY_RELATIVE_RMS)
        {
            return Err(format!(
                "Loop {} waveform differs from the live organ take",
                i + 1
            ));
        }
    }
    if [report.idle, report.stopped]
        .iter()
        .any(|window| window.frames != seq.rate / 4 || window.signal != [0; 2])
    {
        return Err("Expected silent JACK output before recording and after stop".into());
    }
    Ok(())
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => {
            println!(
                "PASS: the native engine recorded and replayed the synth on the JACK device clock"
            );
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("FAIL: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
