//! Test-only: a [`Driver`] with no hardware. Its devices are knobs ([`FakeDevice`]); a started device
//! is a thread that runs the real callback bodies (`callback.rs`) on synthetic time, a fixed factor
//! faster than real time: each cycle the input body, then the output body, entered at the instant a
//! device at that rate would have entered it. A WASAPI output keeps an endpoint buffer as cpal's does:
//! each callback fills what the device played since the last one, the first the whole buffer. One-shot
//! injections (a late wake, an xrun, a missing input, a fatal error, an ASIO input's lead) land on the
//! next cycle or start. The output's left channel is kept by device frame.
//!
//! The ASIO device runs i32 samples (as Focusrite's driver does), WASAPI f32, so both conversions run.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering::{Acquire, Relaxed, Release}};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cpal::{FromSample, Sample, SizedSample};
use lf_engine::grid::Frame;

use super::callback::{Capture, Render, Side, Tap};
use super::driver::{Driver, Mirror, Share, Spec, Started, Streams, Wiring};
use super::{Core, DeviceRequest, IoCounters};
use crate::audio_output::AudioBackend;

/// How much faster than real time a fake device plays: seconds of audio per test, and light enough (one
/// device at a time, `tests.rs`) that the tests beside it keep their timing.
const SPEED: f64 = 4.0;
/// Cycles one wake may catch up (a slow machine renders late rather than all at once).
const MAX_BURST: usize = 64;
/// Frames of the tape kept (the left channel by device frame).
const TAPE_FRAMES: usize = 1 << 22;
/// A WASAPI endpoint buffer in periods (the rig's: 970 frames at a 441-frame period).
const WASAPI_BUFFER_PERIODS: f64 = 2.2;

#[derive(Clone, Debug)]
pub(crate) struct FakeDevice {
    pub(crate) name: String,
    pub(crate) rate: u32,
    /// Frames per callback (the output's; an ASIO request's buffer overrides it).
    pub(crate) block: u32,
    pub(crate) in_channels: usize,
    pub(crate) out_channels: usize,
    /// What the driver reports for each callback, in frames.
    pub(crate) in_latency: u32,
    pub(crate) out_latency: u32,
}

impl FakeDevice {
    pub(crate) fn new(name: &str, rate: u32, block: u32) -> FakeDevice {
        FakeDevice { name: name.to_string(), rate, block, in_channels: 2, out_channels: 2, in_latency: 32, out_latency: 48 }
    }
}

/// A Share mirror the fake opened.
#[derive(Default)]
pub(crate) struct FakeShare {
    pub(crate) endpoint: String,
    /// Frames its tap was fed, and the loudest left sample.
    pub(crate) frames: AtomicU64,
    pub(crate) peak: AtomicU32,
    /// Set by a test: the mirror's stream died.
    pub(crate) faulted: AtomicBool,
    /// The thread its tap was dropped on.
    pub(crate) tap_dropped_on: Mutex<Option<String>>,
}

/// The fake's knobs and what it recorded, shared with the test.
pub(crate) struct Fake {
    /// The ASIO duplex driver (`None` = none cached).
    pub(crate) asio: Mutex<Option<FakeDevice>>,
    /// WASAPI endpoints by id; the first answers a `None` pick (the default).
    pub(crate) wasapi: Mutex<Vec<(String, FakeDevice)>>,
    /// WASAPI: the input clock's drift against the output's, in ppm.
    pub(crate) skew_ppm: Mutex<f64>,
    /// Starts that fail from now on.
    pub(crate) fail_starts: AtomicU32,
    pub(crate) share_fails: AtomicBool,
    /// Streams started so far.
    pub(crate) started: AtomicU32,
    /// One-shot, taken by the running device's next cycle: periods its wake comes late (ASIO: the
    /// bufferSwitches it skips; WASAPI: the input runs on, the output buffer drains), a cpal xrun
    /// report, an input callback that does not run, a fatal error on these streams (`Side` bits).
    pub(crate) gap: AtomicU32,
    pub(crate) xrun: AtomicBool,
    pub(crate) skip_input: AtomicBool,
    pub(crate) fatal: AtomicU8,
    /// One-shot, taken by the next ASIO start: input cycles before the output's first.
    pub(crate) lead_in: AtomicU32,
    /// One-shot, run by the next start before it starts: a test's way into an open midway.
    pub(crate) on_start: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// The device input: channel `c` carries `input(frame) * (c + 1)`.
    input: Mutex<Arc<dyn Fn(Frame) -> f32 + Send + Sync>>,
    /// The left channel played, by device frame (NaN where nothing played).
    pub(crate) tape: Mutex<Vec<f32>>,
    /// The device frame each output callback started at, one list per run.
    pub(crate) starts: Mutex<Vec<Vec<Frame>>>,
    pub(crate) shares: Mutex<Vec<Arc<FakeShare>>>,
}

impl Fake {
    /// An ASIO driver at 48 kHz, 256 frames, and one WASAPI default endpoint at 48 kHz.
    pub(crate) fn new() -> Arc<Fake> {
        Arc::new(Fake {
            asio: Mutex::new(Some(FakeDevice::new("Fake ASIO", 48_000, 256))),
            wasapi: Mutex::new(vec![("default".to_string(), FakeDevice::new("Fake WASAPI", 48_000, 480))]),
            skew_ppm: Mutex::new(0.0),
            fail_starts: AtomicU32::new(0),
            share_fails: AtomicBool::new(false),
            started: AtomicU32::new(0),
            gap: AtomicU32::new(0),
            xrun: AtomicBool::new(false),
            skip_input: AtomicBool::new(false),
            fatal: AtomicU8::new(0),
            lead_in: AtomicU32::new(0),
            on_start: Mutex::new(None),
            input: Mutex::new(Arc::new(|_| 0.0)),
            tape: Mutex::new(Vec::new()),
            starts: Mutex::new(Vec::new()),
            shares: Mutex::new(Vec::new()),
        })
    }

    pub(crate) fn set_input(&self, input: impl Fn(Frame) -> f32 + Send + Sync + 'static) {
        *self.input.lock().unwrap() = Arc::new(input);
    }

    /// The left channel played over `frames` (NaN where nothing played).
    pub(crate) fn heard(&self, frames: std::ops::Range<Frame>) -> Vec<f32> {
        let tape = self.tape.lock().unwrap();
        frames.map(|f| tape.get(f as usize).copied().unwrap_or(f32::NAN)).collect()
    }
}

pub(crate) struct FakeDriver(pub(crate) Arc<Fake>);

/// A resolved fake pair.
pub(crate) struct FakePair {
    input: FakeDevice,
    output: FakeDevice,
}

impl Driver for FakeDriver {
    type Device = FakePair;

    fn resolve(&mut self, request: &DeviceRequest) -> Result<(Spec, FakePair), String> {
        let (input, output) = match request.backend {
            AudioBackend::Asio => {
                let mut device = self.0.asio.lock().unwrap().clone().ok_or("fake: no ASIO driver")?;
                if let Some(block) = request.buffer {
                    device.block = block;
                }
                (device.clone(), device)
            }
            AudioBackend::Wasapi => {
                let endpoints = self.0.wasapi.lock().unwrap();
                let pick = |id: &Option<String>| {
                    match id {
                        None => endpoints.first(),
                        Some(id) => endpoints.iter().find(|(e, _)| e == id),
                    }
                    .map(|(_, d)| d.clone())
                    .ok_or_else(|| format!("fake: no WASAPI endpoint {id:?}"))
                };
                (pick(&request.input)?, pick(&request.output)?)
            }
        };
        let spec = Spec {
            backend: request.backend,
            rate: output.rate,
            in_rate: input.rate,
            in_channels: input.in_channels,
            out_channels: output.out_channels,
            block: output.block,
            input_name: input.name.clone(),
            output_name: output.name.clone(),
        };
        Ok((spec, FakePair { input, output }))
    }

    fn start(&mut self, device: FakePair, spec: &Spec, mut wiring: Wiring) -> Result<Started, String> {
        let hook = self.0.on_start.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
        if self.0.fail_starts.load(Acquire) > 0 {
            self.0.fail_starts.fetch_sub(1, Release);
            return Err("fake: the device did not start".to_string());
        }
        // An endpoint with no input channels plays output only, as a PC with no capture device.
        let capture = if spec.in_channels > 0 { Some(wiring.capture(spec)?) } else { None };
        let render = wiring.render(spec)?;
        let errors = (wiring.on_error(Side::Input), wiring.on_error(Side::Output));
        let stop = Arc::new(AtomicBool::new(false));
        let (fake, core, played, stop_play) = (self.0.clone(), wiring.core.clone(), spec.clone(), stop.clone());
        self.0.starts.lock().unwrap().push(Vec::new());
        let join = std::thread::Builder::new()
            .name("lf-fake-device".into())
            .spawn(move || {
                super::callback::skip_promotion();
                let play = Play { fake, core, spec: played, device, capture, render, stop: stop_play };
                if play.spec.backend.is_asio() {
                    play.run::<i32, _, _>(errors);
                } else {
                    play.run::<f32, _, _>(errors);
                }
            })
            .map_err(|e| e.to_string())?;
        self.0.started.fetch_add(1, Relaxed);
        let input_open = spec.in_channels > 0;
        Ok(Started { streams: Streams::new(input_open.then(|| Box::new(()) as Box<dyn Send>), Box::new(Running { stop, join: Some(join) })), block: spec.block, input_open })
    }

    fn open_share(&mut self, endpoint: &str, _rate: u32, _block: u32, _core: &Arc<Core>) -> Result<Share, String> {
        if self.0.share_fails.load(Acquire) {
            return Err(format!("fake: no endpoint {endpoint}"));
        }
        let share = Arc::new(FakeShare { endpoint: endpoint.to_string(), ..FakeShare::default() });
        self.0.shares.lock().unwrap().push(share.clone());
        Ok((Box::new(FakeMirror(share.clone())), Box::new(FakeTap(share))))
    }
}

/// A started fake: dropping it stops and joins its thread, so no callback runs after the drop.
struct Running {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

struct Play {
    fake: Arc<Fake>,
    core: Arc<Core>,
    spec: Spec,
    device: FakePair,
    capture: Option<Capture>,
    render: Render,
    stop: Arc<AtomicBool>,
}

impl Play {
    fn run<T, EI, EO>(mut self, (mut input_error, mut output_error): (EI, EO))
    where
        T: SizedSample + FromSample<f32>,
        f32: FromSample<T>,
        EI: FnMut(cpal::Error),
        EO: FnMut(cpal::Error),
    {
        let (rate, block) = (self.spec.rate, self.spec.block.max(1) as usize);
        let asio = self.spec.backend.is_asio();
        // WASAPI: the endpoint buffer and the frames queued in it.
        let buffer = if asio { block } else { (block as f64 * WASAPI_BUFFER_PERIODS) as usize };
        let mut queued: Option<usize> = None;
        let (in_ch, out_ch) = (self.spec.in_channels.max(1), self.spec.out_channels.max(1));
        let latency = |frames: u32, rate: u32| Some(Duration::from_secs_f64(frames as f64 / rate as f64));
        let (in_latency, out_latency) = (latency(self.device.input.in_latency, self.spec.in_rate), latency(self.device.output.out_latency, rate));
        let skew = *self.fake.skew_ppm.lock().unwrap();
        let in_per_out = self.spec.in_rate as f64 / rate as f64 * (1.0 + skew / 1e6);
        let mut data_in = vec![T::EQUILIBRIUM; (2 * block + 16) * in_ch];
        let mut data_out = vec![T::EQUILIBRIUM; buffer * out_ch];
        let (mut t, mut in_frame, mut carry) = (0u64, 0 as Frame, 0.0f64);
        if asio {
            for _ in 0..self.fake.lead_in.swap(0, Relaxed) {
                self.capture_period(&mut data_in, &mut in_frame, block, in_ch, in_latency);
            }
        }
        let begun = Instant::now();
        let mut dead = false;
        while !self.stop.load(Acquire) {
            let ahead = (begun.elapsed().as_secs_f64() * rate as f64 * SPEED) as u64;
            let mut burst = 0;
            while !dead && t < ahead && burst < MAX_BURST {
                let fatal = self.fake.fatal.swap(0, Relaxed);
                if fatal != 0 {
                    if fatal & Side::Input as u8 != 0 {
                        input_error(cpal::Error::new(cpal::ErrorKind::DeviceNotAvailable));
                    }
                    if fatal & Side::Output as u8 != 0 {
                        output_error(cpal::Error::new(cpal::ErrorKind::DeviceNotAvailable));
                    }
                    dead = true;
                    break;
                }
                if self.fake.xrun.swap(false, Relaxed) {
                    output_error(cpal::Error::new(cpal::ErrorKind::Xrun));
                }
                let late = self.fake.gap.swap(0, Relaxed) as usize;
                t += (late * block) as u64;
                // ASIO: the skipped bufferSwitches took their input with them; WASAPI's input ran on.
                let periods = if asio { 1 } else { 1 + late };
                for period in 0..periods {
                    let k = if asio {
                        block
                    } else {
                        carry += block as f64 * in_per_out;
                        let k = carry.floor();
                        carry -= k;
                        k as usize
                    };
                    if period == 0 && self.fake.skip_input.swap(false, Relaxed) {
                        in_frame += k as Frame;
                    } else {
                        self.capture_period(&mut data_in, &mut in_frame, k, in_ch, in_latency);
                    }
                }
                let n = match queued {
                    None => buffer,
                    Some(q) => buffer - q.saturating_sub((1 + late) * block),
                };
                queued = (!asio).then_some(buffer);
                let entry = begun + Duration::from_secs_f64(t as f64 / rate as f64);
                self.render.render(&mut data_out[..n * out_ch], entry, out_latency);
                let frame = self.core.frame.load(Relaxed) - n as Frame;
                self.keep(frame, &data_out[..n * out_ch], out_ch);
                t += block as u64;
                burst += 1;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// One input callback of `k` frames from the device input, from `in_frame` on.
    fn capture_period<T>(&mut self, data_in: &mut [T], in_frame: &mut Frame, k: usize, in_ch: usize, latency: Option<Duration>)
    where
        T: SizedSample + FromSample<f32>,
        f32: FromSample<T>,
    {
        let input = self.fake.input.lock().unwrap().clone();
        for (i, frame) in data_in[..k * in_ch].chunks_exact_mut(in_ch).enumerate() {
            let x = input(*in_frame + i as Frame);
            for (c, s) in frame.iter_mut().enumerate() {
                *s = T::from_sample(x * (c + 1) as f32);
            }
        }
        if let Some(capture) = self.capture.as_mut() {
            capture.capture(&data_in[..k * in_ch], latency);
        }
        *in_frame += k as Frame;
    }

    fn keep<T: Sample>(&self, frame: Frame, data: &[T], channels: usize)
    where
        f32: FromSample<T>,
    {
        self.fake.starts.lock().unwrap().last_mut().expect("a run's list").push(frame);
        let mut tape = self.fake.tape.lock().unwrap();
        let end = (frame as usize + data.len() / channels).min(TAPE_FRAMES);
        if tape.len() < end {
            tape.resize(end, f32::NAN);
        }
        for (i, s) in data.chunks_exact(channels).enumerate() {
            if let Some(slot) = tape.get_mut(frame as usize + i) {
                *slot = f32::from_sample(s[0]);
            }
        }
    }
}

struct FakeTap(Arc<FakeShare>);

impl Tap for FakeTap {
    fn push(&mut self, left: &[f32], _right: &[f32], _counters: &IoCounters) {
        self.0.frames.fetch_add(left.len() as u64, Relaxed);
        let peak = left.iter().fold(f32::from_bits(self.0.peak.load(Relaxed)), |m, s| m.max(s.abs()));
        self.0.peak.store(peak.to_bits(), Relaxed);
    }
}

impl Drop for FakeTap {
    fn drop(&mut self) {
        *self.0.tap_dropped_on.lock().unwrap() = std::thread::current().name().map(str::to_string);
    }
}

struct FakeMirror(Arc<FakeShare>);

impl Mirror for FakeMirror {
    fn faulted(&self) -> bool {
        self.0.faulted.load(Acquire)
    }
}
