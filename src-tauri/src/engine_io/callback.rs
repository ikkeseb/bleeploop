//! OWNS: the device callbacks' bodies, shared by ASIO, WASAPI and the fake driver: the output callback
//! that renders the engine on the one clock ([`Render`]), the input callbacks that feed it ([`Capture`]:
//! ASIO's same-cycle handoff, WASAPI's join pipe), and what one run's callbacks share with the owner
//! ([`Run`]). A run is one pair of streams, from their start to their drop.
//!
//! Each body takes plain values (the device's samples, the instant the callback entered, the latency
//! the driver reported for it), so the fake driver runs them on synthetic time. The engine lock is only
//! `try_lock`ed, and everything under it runs in [`guarded`]: a panic is caught inside the guard, so the
//! lock is never poisoned and nothing unwinds into the driver (asio-sys's `extern "C"` bufferSwitch
//! would abort).

use std::cell::Cell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicU8, Ordering::{Acquire, Relaxed, Release}};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use cpal::{FromSample, Sample, SizedSample};
use lf_engine::grid::Frame;
use lf_engine::ProcessContext;
use rtrb::{Consumer, Producer};

use super::pipes::{PullPipe, PushEnd};
use super::{Core, IoCounters, Rt};

/// The largest device callback the bodies take whole, in frames (the ASIO handoff, the join's pull
/// buffer). The excess of a larger one renders from silence: an input gap, never out of bounds.
pub(crate) const MAX_DEVICE_BLOCK: usize = 8192;
/// The fade on a switch, out and in: the web monitor's declick (`audio_output.rs`).
const FADE_SECONDS: f64 = 0.010;
/// Silent callbacks after a fade-out before the owner may drop the streams: a dropped ASIO stream only
/// loses its callback, and the driver keeps playing its two buffer halves.
const SILENT_BLOCKS: u32 = 2;
/// Latency reports whose median the alignment takes before it freezes for the run.
pub(crate) const LATENCY_SAMPLES: usize = 16;
/// An input sample at or past this reached full scale (the meter's clip).
const CLIP_LEVEL: f32 = 0.999;

/// Share output's end in the callback (`share::ShareTap`; a fake in tests): fed the rendered stereo
/// master every block. Never blocks or allocates.
pub(crate) trait Tap: Send {
    fn push(&mut self, left: &[f32], right: &[f32], counters: &IoCounters);
}

/// The callback's end of the handoff that brings Share output's tap in while streams run: the owner
/// sends the new tap (or none), and the callback hands the old one back for the owner to drop.
pub(crate) struct TapEnd {
    pub(crate) rx: Consumer<Option<Box<dyn Tap>>>,
    pub(crate) back: Producer<Box<dyn Tap>>,
}

impl Rt {
    /// At a block start: take a tap the owner sent. Waits (the message stays) while the return ring is
    /// full, so an old tap never has to be dropped here.
    fn poll_tap(&mut self) {
        let Some(end) = self.taps.as_mut() else { return };
        if end.back.slots() == 0 {
            return;
        }
        if let Ok(next) = end.rx.pop() {
            if let Some(old) = std::mem::replace(&mut self.tap, next) {
                // Room was checked above; the ring has one consumer, so it cannot fill meanwhile.
                let _ = end.back.push(old);
            }
        }
    }
}

/// Which stream an error came from.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Side {
    Input = 1,
    Output = 2,
}

/// What one run's callbacks share with the owner.
pub(crate) struct Run {
    /// The capture channel the input callback reads (`EngineHost::set_input_channel`).
    pub(crate) channel: AtomicU32,
    /// The owner asks for the fade-out; the output callback answers once the device plays silence.
    pub(crate) fade_out: AtomicBool,
    pub(crate) faded: AtomicBool,
    /// Fatal stream errors (`Side` bits) and the first one, for the owner to report.
    pub(crate) fault: AtomicU8,
    pub(crate) error: Mutex<Option<cpal::Error>>,
    /// cpal reported an xrun: the next block follows an input gap.
    xrun: AtomicBool,
    /// Each side's latency in frames at the engine rate: a running median, frozen after
    /// `LATENCY_SAMPLES` reports.
    in_latency: AtomicI64,
    out_latency: AtomicI64,
    /// Output callbacks this run.
    pub(crate) callbacks: AtomicU64,
}

impl Run {
    pub(crate) fn new(channel: u32) -> Run {
        Run {
            channel: AtomicU32::new(channel),
            fade_out: AtomicBool::new(false),
            faded: AtomicBool::new(false),
            fault: AtomicU8::new(0),
            error: Mutex::new(None),
            xrun: AtomicBool::new(false),
            in_latency: AtomicI64::new(0),
            out_latency: AtomicI64::new(0),
            callbacks: AtomicU64::new(0),
        }
    }

    /// A stream's error callback. An xrun is counted and flags the next block. cpal 0.18.1 documents a
    /// default-device change (the stream stays on its device) and a refused thread priority as
    /// non-fatal; any other error ends the stream: latched for the owner, which drops the run and falls
    /// back. Never blocks (an xrun can arrive on the driver's thread).
    pub(crate) fn stream_error(&self, side: Side, error: cpal::Error, counters: &IoCounters) {
        match error.kind() {
            cpal::ErrorKind::Xrun => {
                counters.xruns.fetch_add(1, Relaxed);
                self.xrun.store(true, Release);
            }
            cpal::ErrorKind::DeviceChanged | cpal::ErrorKind::RealtimeDenied => {}
            _ => {
                if let Ok(mut first) = self.error.try_lock() {
                    if first.is_none() {
                        *first = Some(error);
                    }
                }
                self.fault.fetch_or(side as u8, Release);
            }
        }
    }

    pub(crate) fn faulted(&self) -> bool {
        self.fault.load(Acquire) != 0
    }

    /// The alignment the callbacks measured so far: (input + output, input), in frames.
    pub(crate) fn latency(&self) -> (Frame, Frame) {
        let (input, output) = (self.in_latency.load(Relaxed), self.out_latency.load(Relaxed));
        (input + output, input)
    }
}

/// The median of a run's first `LATENCY_SAMPLES` latency reports, then frozen.
struct Probe {
    values: [Frame; LATENCY_SAMPLES],
    count: usize,
}

impl Probe {
    const fn new() -> Probe {
        Probe { values: [0; LATENCY_SAMPLES], count: 0 }
    }

    /// Take one report (a duration within one stream's timestamps): the median so far in frames at
    /// `rate`, or `None` once frozen or for a report that is no latency (missing, or over a second).
    fn observe(&mut self, latency: Option<Duration>, rate: u32) -> Option<Frame> {
        if self.count == LATENCY_SAMPLES {
            return None;
        }
        let latency = latency.filter(|d| *d <= Duration::from_secs(1))?;
        self.values[self.count] = (latency.as_secs_f64() * rate as f64).round() as Frame;
        self.count += 1;
        let mut sorted = self.values;
        let s = &mut sorted[..self.count];
        s.sort_unstable();
        Some((s[(self.count - 1) / 2] + s[self.count / 2]) / 2)
    }
}

thread_local! {
    static PROMOTED: Cell<bool> = const { Cell::new(false) };
}

/// MMCSS Pro Audio for the calling thread, once (every engine, join and share callback thread).
fn promote_once() {
    if !PROMOTED.with(|p| p.replace(true)) {
        let _ = super::promote_pro_audio();
    }
}

/// Test-only: the fake driver's thread renders faster than real time and must not outrank the threads
/// of the tests running beside it.
#[cfg(test)]
pub(crate) fn skip_promotion() {
    PROMOTED.with(|p| p.set(true));
}

/// The engine lock for a callback, or `None` when another thread holds it (a miss). A poisoned lock is
/// taken anyway: nothing under it panics uncaught, and a poison from elsewhere must not silence the
/// device for good.
fn try_rt(core: &Core) -> Option<MutexGuard<'_, Rt>> {
    match core.rt.try_lock() {
        Ok(rt) => Some(rt),
        Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

/// Run `body` as a callback's guarded section: a panic is caught and counted (false), and in DEV builds
/// the allocations inside it are counted (`host::rt_alloc`; the counter is process-wide, so another
/// thread's guarded allocation at the same moment would count here too).
pub(crate) fn guarded(counters: &IoCounters, body: impl FnOnce()) -> bool {
    #[cfg(debug_assertions)]
    let before = crate::host::rt_alloc::RT_ALLOCS.load(Relaxed);
    #[cfg(debug_assertions)]
    let alloc_guard = crate::host::rt_alloc::guard();
    let ok = catch_unwind(AssertUnwindSafe(body)).is_ok();
    #[cfg(debug_assertions)]
    {
        drop(alloc_guard);
        let after = crate::host::rt_alloc::RT_ALLOCS.load(Relaxed);
        if after > before {
            counters.rt_allocs.fetch_add(after - before, Relaxed);
        }
    }
    if !ok {
        counters.panics.fetch_add(1, Relaxed);
    }
    ok
}

/// An input callback's body.
pub(crate) enum Capture {
    Duplex(DuplexInput),
    Join(JoinInput),
}

impl Capture {
    pub(crate) fn capture<T: SizedSample>(&mut self, data: &[T], latency: Option<Duration>)
    where
        f32: FromSample<T>,
    {
        match self {
            Capture::Duplex(input) => input.capture(data, latency),
            Capture::Join(input) => input.capture(data, latency),
        }
    }
}

/// ASIO's input callback: asio-sys runs it first in every bufferSwitch (it registered first), so it
/// copies the selected channel into the handoff for this cycle's output callback and counts the cycle.
pub(crate) struct DuplexInput {
    core: Arc<Core>,
    run: Arc<Run>,
    channels: usize,
    rate: u32,
    probe: Probe,
}

impl DuplexInput {
    pub(crate) fn new(core: Arc<Core>, run: Arc<Run>, channels: usize, rate: u32) -> DuplexInput {
        DuplexInput { core, run, channels: channels.max(1), rate, probe: Probe::new() }
    }

    fn capture<T: SizedSample>(&mut self, data: &[T], latency: Option<Duration>)
    where
        f32: FromSample<T>,
    {
        promote_once();
        if let Some(frames) = self.probe.observe(latency, self.rate) {
            self.run.in_latency.store(frames, Relaxed);
        }
        let Some(mut rt) = try_rt(&self.core) else {
            self.core.counters.lock_misses.fetch_add(1, Relaxed);
            return;
        };
        let (channels, channel) = (self.channels, (self.run.channel.load(Relaxed) as usize).min(self.channels - 1));
        let rt = &mut *rt;
        let ok = guarded(&self.core.counters, || {
            let n = data.len() / channels;
            for (h, frame) in rt.handoff.iter_mut().zip(data.chunks_exact(channels)) {
                *h = f32::from_sample(frame[channel]);
            }
            rt.handoff_len = n;
            rt.in_cycles += 1;
        });
        if !ok {
            self.core.latch_fault(rt);
        }
    }
}

/// WASAPI's input callback, on its own thread and clock: pushes the selected channel into the join
/// pipe the output callback pulls from (drop-on-full, counted), and measures the capture's age.
pub(crate) struct JoinInput {
    core: Arc<Core>,
    run: Arc<Run>,
    channels: usize,
    /// The engine's rate: the age is measured in its frames.
    rate: u32,
    push: PushEnd,
    scratch: Vec<f32>,
    probe: Probe,
    /// A caught panic: the input stays silent for the rest of the run.
    dead: bool,
}

impl JoinInput {
    /// Allocates: build it on the owner thread.
    pub(crate) fn new(core: Arc<Core>, run: Arc<Run>, channels: usize, rate: u32, push: PushEnd) -> JoinInput {
        JoinInput { core, run, channels: channels.max(1), rate, push, scratch: vec![0.0; 1024], probe: Probe::new(), dead: false }
    }

    fn capture<T: SizedSample>(&mut self, data: &[T], latency: Option<Duration>)
    where
        f32: FromSample<T>,
    {
        promote_once();
        #[cfg(debug_assertions)]
        trace::join_push(data.len() / self.channels);
        if let Some(frames) = self.probe.observe(latency, self.rate) {
            self.run.in_latency.store(frames, Relaxed);
        }
        if self.dead {
            return;
        }
        let (channels, channel) = (self.channels, (self.run.channel.load(Relaxed) as usize).min(self.channels - 1));
        let (push, scratch, counters) = (&mut self.push, &mut self.scratch, &self.core.counters);
        let ok = guarded(counters, || {
            let mut dropped = 0;
            for chunk in data.chunks(channels * scratch.len()) {
                let mono = &mut scratch[..chunk.len() / channels];
                for (s, frame) in mono.iter_mut().zip(chunk.chunks_exact(channels)) {
                    *s = f32::from_sample(frame[channel]);
                }
                dropped += push.push(mono);
            }
            if dropped > 0 {
                counters.join_overruns.fetch_add(1, Relaxed);
            }
        });
        self.dead = !ok;
    }
}

/// Where the output callback's input comes from.
enum Source {
    /// ASIO: this cycle's handoff in `Rt`.
    Duplex,
    /// WASAPI: the join pipe, pulled to exactly this block. `joined` once a pull came back whole: the
    /// startup pulls before the input arrives are not starves.
    Join { pipe: PullPipe, x: Vec<f32>, joined: bool, delay: Option<Frame> },
}

/// The output callback: the one clock. It numbers the device frames, renders the engine in slices of
/// at most its `max_block`, writes stereo to the device, fades on a switch, feeds Share output's tap,
/// publishes the frame clock and mirrors the engine's counters.
pub(crate) struct Render {
    core: Arc<Core>,
    run: Arc<Run>,
    channels: usize,
    rate: u32,
    source: Source,
    left: Vec<f32>,
    right: Vec<f32>,
    zeros: Vec<f32>,
    /// The previous callback's entry and frames (WASAPI's dry buffer, the DEV trace).
    last: Option<(Instant, usize)>,
    /// WASAPI: the endpoint buffer, the largest callback this run (the first finds it empty).
    cap: usize,
    /// The previous block never reached the engine (a lock miss): this one follows an input gap.
    missed: bool,
    gain: f32,
    step: f32,
    silent: u32,
    probe: Probe,
}

impl Render {
    /// ASIO's output callback. Allocates: build it on the owner thread, after the engine exists.
    pub(crate) fn duplex(core: Arc<Core>, run: Arc<Run>, channels: usize, rate: u32) -> Render {
        Render::new(core, run, channels, rate, Source::Duplex)
    }

    /// WASAPI's output callback, pulling the input through the join pipe.
    pub(crate) fn join(core: Arc<Core>, run: Arc<Run>, channels: usize, rate: u32, pipe: PullPipe) -> Render {
        let x = vec![0.0; MAX_DEVICE_BLOCK];
        Render::new(core, run, channels, rate, Source::Join { pipe, x, joined: false, delay: None })
    }

    fn new(core: Arc<Core>, run: Arc<Run>, channels: usize, rate: u32, source: Source) -> Render {
        let max_block = (core.max_block.load(Relaxed) as usize).max(1);
        Render {
            core,
            run,
            channels: channels.max(1),
            rate,
            source,
            left: vec![0.0; max_block],
            right: vec![0.0; max_block],
            zeros: vec![0.0; max_block],
            last: None,
            cap: 0,
            missed: false,
            gain: 0.0,
            step: (1.0 / (FADE_SECONDS * rate as f64).max(1.0)) as f32,
            silent: 0,
            probe: Probe::new(),
        }
    }

    /// One output callback: `data` interleaved at the device's channel count, `entry` when the callback
    /// entered, `latency` the playback delay this stream reported for it.
    pub(crate) fn render<T: SizedSample + FromSample<f32>>(&mut self, data: &mut [T], entry: Instant, latency: Option<Duration>) {
        // Its own clock, not `entry`: the fake driver's entries are synthetic.
        let began = Instant::now();
        promote_once();
        let core = Arc::clone(&self.core);
        let counters = &core.counters;
        counters.callbacks.fetch_add(1, Relaxed);
        self.run.callbacks.fetch_add(1, Relaxed);
        let n = data.len() / self.channels;

        #[cfg(debug_assertions)]
        trace::output(self.last, entry, n, latency, self.rate, self.run.callbacks.load(Relaxed));
        // The frames the device played that this run never rendered. Only WASAPI can say (`dry_frames`).
        // ASIO infers nothing from timing: each bufferSwitch carries one period in and out, so the
        // counter counts what the device took, and a late wake the driver makes up loses nothing (at 64
        // frames the rig's driver woke 98, 78 and 8 frames apart). A period the driver drops arrives
        // as its overload report, a cpal Xrun.
        self.cap = self.cap.max(n);
        let lost = match (self.last.replace((entry, n)), &self.source) {
            (Some((before, _)), Source::Join { .. }) => dry_frames(entry.saturating_duration_since(before), self.rate, n, self.cap),
            _ => 0,
        };
        if lost > 0 {
            counters.gaps.fetch_add(1, Relaxed);
        }
        if let Some(frames) = self.probe.observe(latency, self.rate) {
            self.run.out_latency.store(frames, Relaxed);
        }
        // The join pipe is this callback's own: pull even when the engine is locked, so its fill holds.
        let avail = match &mut self.source {
            Source::Duplex => 0,
            Source::Join { pipe, x, joined, .. } => {
                let m = n.min(x.len());
                // The input captured while the buffer played dry belongs to the frames skipped below.
                pipe.skip(lost as usize);
                #[cfg(debug_assertions)]
                let fill_before = pipe.fill();
                let zeroed = pipe.pull(&mut x[..m]) + (n - m);
                let trims = pipe.take_trims();
                #[cfg(debug_assertions)]
                trace::join_pull(self.run.callbacks.load(Relaxed), n, fill_before, trims > 0);
                if trims > 0 {
                    counters.join_trims.fetch_add(trims, Relaxed);
                }
                if zeroed == 0 {
                    *joined = true;
                } else if *joined {
                    counters.join_starves.fetch_add(1, Relaxed);
                }
                m
            }
        };
        let frame = core.frame.load(Relaxed) + lost;
        let xrun = (lost > 0) | std::mem::take(&mut self.missed) | self.run.xrun.swap(false, Acquire);
        let (align, input_frames) = self.alignment();
        core.align_frames.store(align, Relaxed);
        core.input_frames.store(input_frames, Relaxed);
        let fading = self.run.fade_out.load(Acquire);
        let silent_start = fading && self.gain == 0.0;
        // Before the render: a press that arrives while this block renders maps to this block.
        core.clock.publish(entry, frame, n as u32, self.rate);

        match try_rt(&core) {
            Some(mut rt) => {
                let rt = &mut *rt;
                let ctx = ProcessContext { frame, xrun, align_frames: align, input_frames };
                let ok = guarded(counters, || self.block(rt, data, n, avail, ctx, fading));
                if !ok {
                    core.latch_fault(rt);
                    self.quiet(data, fading);
                }
            }
            None => {
                counters.lock_misses.fetch_add(1, Relaxed);
                self.missed = true;
                self.quiet(data, fading);
            }
        }
        if silent_start {
            self.silent += 1;
            if self.silent >= SILENT_BLOCKS {
                self.run.faded.store(true, Release);
            }
        }
        core.frame.store(frame + n as Frame, Relaxed);
        counters.block_load.record(began.elapsed(), n, self.rate);
    }

    /// A block the engine did not render plays silence; a fade-out in progress has reached it.
    fn quiet<T: Sample>(&mut self, data: &mut [T], fading: bool) {
        silence(data);
        if fading {
            self.gain = 0.0;
        }
    }

    /// The block under the engine lock: this cycle's input, the engine in slices, the fade, Share output
    /// and the device's channels.
    fn block<T: SizedSample + FromSample<f32>>(&mut self, rt: &mut Rt, data: &mut [T], n: usize, avail: usize, ctx: ProcessContext, fading: bool) {
        rt.poll_tap();
        let counters = &self.core.counters;
        let avail = match self.source {
            Source::Duplex => {
                // A run's first output callback takes the input's count: the input starts playing just
                // before the output, so the cycles it ran alone are the start, not a fault.
                rt.out_cycles = if rt.out_cycles == 0 { rt.in_cycles } else { rt.out_cycles + 1 };
                let same = rt.in_cycles == rt.out_cycles && rt.handoff_len == n && n <= rt.handoff.len();
                if !same && rt.out_cycles > 0 {
                    // Out of step: count it once and resync, as the Stage 1 spike does.
                    counters.duplex_faults.fetch_add(1, Relaxed);
                    rt.out_cycles = rt.in_cycles;
                }
                if same { n } else { 0 }
            }
            Source::Join { .. } => avail,
        };
        let Rt {
            engine,
            faulted,
            handoff,
            tap,
            #[cfg(debug_assertions)]
            lag,
            ..
        } = rt;
        let input: &[f32] = match &self.source {
            Source::Duplex => &handoff[..],
            Source::Join { x, .. } => &x[..],
        };
        meter(&self.core, &input[..avail.min(input.len())]);
        let mut engine = engine.as_mut().filter(|_| !*faulted);
        let max_block = self.left.len();
        let target = if fading { 0.0 } else { 1.0 };
        let mut off = 0;
        while off < n {
            let m = (n - off).min(max_block);
            let x = if off + m <= avail { &input[off..off + m] } else { &self.zeros[..m] };
            let (left, right) = (&mut self.left[..m], &mut self.right[..m]);
            match engine.as_deref_mut() {
                Some(engine) => {
                    let ctx = ProcessContext { frame: ctx.frame + off as Frame, xrun: ctx.xrun && off == 0, ..ctx };
                    engine.process(&ctx, x, left, right);
                }
                None => {
                    left.fill(0.0);
                    right.fill(0.0);
                }
            }
            fade(&mut self.gain, self.step, target, left, right);
            #[cfg(debug_assertions)]
            if let Some(lag) = lag.as_mut() {
                lag.block(ctx.frame + off as Frame, x, left, right);
            }
            if let Some(tap) = tap.as_mut() {
                tap.push(left, right, counters);
            }
            write(&mut data[off * self.channels..(off + m) * self.channels], self.channels, left, right);
            off += m;
        }
        if let Some(engine) = engine {
            self.core.engine_diag.store(&engine.diag());
        }
    }

    /// The alignment for this block: (input + output, input) in frames. WASAPI's input side adds the
    /// join pipe's settled delay to the capture's age.
    fn alignment(&mut self) -> (Frame, Frame) {
        let (align, input) = self.run.latency();
        let delay = match &mut self.source {
            Source::Duplex => 0,
            Source::Join { pipe, delay, .. } => *delay.get_or_insert_with(|| pipe.delay_frames().round() as Frame),
        };
        (align + delay, input + delay)
    }
}

/// Fold a block of input into the meter the feed takes: its peak, and a clip.
fn meter(core: &Core, input: &[f32]) {
    let peak = input.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    if peak > 0.0 {
        // A non-negative f32's bits order as the value does.
        core.meter_peak.fetch_max(peak.to_bits(), Relaxed);
    }
    if peak >= CLIP_LEVEL {
        core.meter_clip.store(true, Relaxed);
    }
}

/// WASAPI: the frames the device played from an empty buffer between the previous callback, `elapsed`
/// before this one, and this one of `n` frames. Every callback fills the endpoint buffer (`cap` frames:
/// the run's first finds it empty and takes it whole), so it ran dry only if this callback finds it
/// empty again, for as long as the device played past what it held. A late callback that finds frames
/// still queued lost nothing, however late (the rig's n = 441 behind a late wake: the audio engine was
/// late and caught up two callbacks later).
fn dry_frames(elapsed: Duration, rate: u32, n: usize, cap: usize) -> Frame {
    if n < cap {
        return 0;
    }
    let played = elapsed.as_secs_f64() * rate as f64;
    (played - cap as f64).round().max(0.0) as Frame
}

/// DEV: what the counters cannot say, for the rig probe (`probe.rs` prints it). Each run's first output
/// callback (WASAPI: its buffer), each late wake (more than 1.5 periods after the previous) and the two
/// callbacks after it (a late wake the device makes up, or a buffer run dry), each join trim with the fill it found, and every 1000th join output
/// callback the frames the input pushed and the output pulled so far (their rates, against QPC).
#[cfg(debug_assertions)]
pub(crate) mod trace {
    use std::cell::Cell;
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering::Relaxed};
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    const LEN: usize = 1024;
    static RECORDS: [[AtomicI64; 6]; LEN] = [const { [const { AtomicI64::new(0) }; 6] }; LEN];
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    static IN_FRAMES: AtomicI64 = AtomicI64::new(0);
    static OUT_FRAMES: AtomicI64 = AtomicI64::new(0);
    static EPOCH: OnceLock<Instant> = OnceLock::new();

    thread_local! {
        static AFTER: Cell<u8> = const { Cell::new(0) };
    }

    fn put(r: [i64; 6]) {
        let i = NEXT.fetch_add(1, Relaxed);
        if i < LEN {
            for (a, v) in RECORDS[i].iter().zip(r) {
                a.store(v, Relaxed);
            }
        }
    }

    /// An output callback of `n` frames, entered at `entry`, after `last` (the previous entry and its
    /// frames).
    pub(crate) fn output(last: Option<(Instant, usize)>, entry: Instant, n: usize, latency: Option<Duration>, rate: u32, callback: u64) {
        let (kind, elapsed, prev) = match last {
            None => (0, -1, 0),
            Some((before, prev)) => {
                let e = entry.saturating_duration_since(before).as_secs_f64() * rate as f64;
                if prev > 0 && e > 1.5 * prev as f64 {
                    AFTER.set(2);
                    (1, e.round() as i64, prev)
                } else if AFTER.get() > 0 {
                    AFTER.set(AFTER.get() - 1);
                    (2, e.round() as i64, prev)
                } else {
                    return;
                }
            }
        };
        let latency = latency.map_or(-1, |d| (d.as_secs_f64() * rate as f64).round() as i64);
        put([kind, callback as i64, elapsed, prev as i64, n as i64, latency]);
    }

    /// A join output callback that pulled `n` frames from a ring holding `fill` (frames at the input's
    /// rate), and whether that pull trimmed it.
    pub(crate) fn join_pull(callback: u64, n: usize, fill: usize, trimmed: bool) {
        let out = OUT_FRAMES.fetch_add(n as i64, Relaxed) + n as i64;
        if trimmed {
            put([3, callback as i64, 0, 0, n as i64, fill as i64]);
        }
        if callback % 1000 == 0 {
            let ms = EPOCH.get_or_init(Instant::now).elapsed().as_millis() as i64;
            put([4, callback as i64, ms, IN_FRAMES.load(Relaxed), out, fill as i64]);
        }
    }

    /// A join input callback of `n` frames.
    pub(crate) fn join_push(n: usize) {
        IN_FRAMES.fetch_add(n as i64, Relaxed);
    }

    /// The records so far, one line each.
    pub(crate) fn lines() -> Vec<String> {
        (0..NEXT.load(Relaxed).min(LEN))
            .map(|i| {
                let [kind, cb, a, b, c, d] = std::array::from_fn(|k| RECORDS[i][k].load(Relaxed));
                match kind {
                    0 => format!("first callback {cb}: n={c} latency={d}"),
                    1 | 2 => format!("{} callback {cb}: elapsed={a} prev={b} n={c} latency={d}", if kind == 1 { "LATE " } else { "after" }),
                    3 => format!("TRIM  callback {cb}: n={c} fill={d}"),
                    _ => format!("join  callback {cb}: {a} ms, pushed {b}, pulled {c}, fill {d}"),
                }
            })
            .collect()
    }
}

/// Ramp the block toward `target` (0 = silence, 1 = full), a linear step per frame.
fn fade(gain: &mut f32, step: f32, target: f32, left: &mut [f32], right: &mut [f32]) {
    if *gain == target {
        if target == 0.0 {
            left.fill(0.0);
            right.fill(0.0);
        }
        return;
    }
    for (l, r) in left.iter_mut().zip(right.iter_mut()) {
        *gain = if target > *gain { (*gain + step).min(target) } else { (*gain - step).max(target) };
        *l *= *gain;
        *r *= *gain;
    }
}

/// Stereo out: left and right on the device's first two channels, any others silent; a mono device
/// gets their mean.
fn write<T: SizedSample + FromSample<f32>>(data: &mut [T], channels: usize, left: &[f32], right: &[f32]) {
    for ((frame, &l), &r) in data.chunks_exact_mut(channels).zip(left).zip(right) {
        match frame {
            [mono] => *mono = T::from_sample(0.5 * (l + r)),
            [a, b, rest @ ..] => {
                *a = T::from_sample(l);
                *b = T::from_sample(r);
                rest.fill(T::EQUILIBRIUM);
            }
            [] => {}
        }
    }
}

fn silence<T: Sample>(data: &mut [T]) {
    data.fill(T::EQUILIBRIUM);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_lands_on_the_first_two_channels_and_a_mono_device_gets_the_mean() {
        let (left, right) = ([0.5f32, -0.25], [0.25f32, 0.75]);
        let mut quad = [9.0f32; 8];
        write(&mut quad, 4, &left, &right);
        assert_eq!(quad, [0.5, 0.25, 0.0, 0.0, -0.25, 0.75, 0.0, 0.0]);
        let mut mono = [0i16; 2];
        write(&mut mono, 1, &left, &right);
        assert_eq!(mono, [i16::from_sample(0.375f32), i16::from_sample(0.25f32)]);
        let mut stereo = [0i32; 4];
        write(&mut stereo, 2, &left, &right);
        assert_eq!(stereo, [i32::from_sample(0.5f32), i32::from_sample(0.25f32), i32::from_sample(-0.25f32), i32::from_sample(0.75f32)]);
        let mut unsigned = [0u16; 2];
        silence(&mut unsigned);
        assert_eq!(unsigned, [u16::EQUILIBRIUM; 2]);
    }

    #[test]
    fn only_a_buffer_found_empty_lost_frames_and_only_what_played_past_it() {
        let frames = |k: f64| Duration::from_secs_f64(k / 48_000.0);
        assert_eq!(dry_frames(frames(441.0), 48_000, 441, 970), 0, "on time");
        assert_eq!(dry_frames(frames(1_300.0), 48_000, 882, 970), 0, "late, but frames were still queued");
        assert_eq!(dry_frames(frames(960.0), 48_000, 970, 970), 0, "empty just as it came");
        assert_eq!(dry_frames(frames(1_323.0), 48_000, 970, 970), 353, "dry for what played past the buffer");
    }

    #[test]
    fn the_fade_ramps_both_ways_and_holds_silence() {
        let step = 0.25;
        let (mut l, mut r) = ([1.0f32; 6], [1.0f32; 6]);
        let mut gain = 0.0;
        fade(&mut gain, step, 1.0, &mut l, &mut r);
        assert_eq!(l, [0.25, 0.5, 0.75, 1.0, 1.0, 1.0]);
        let (mut l, mut r) = ([1.0f32; 6], [1.0f32; 6]);
        fade(&mut gain, step, 0.0, &mut l, &mut r);
        assert_eq!(r, [0.75, 0.5, 0.25, 0.0, 0.0, 0.0]);
        let (mut l, mut r) = ([1.0f32; 2], [1.0f32; 2]);
        fade(&mut gain, step, 0.0, &mut l, &mut r);
        assert_eq!((l, r), ([0.0; 2], [0.0; 2]));
    }

    #[test]
    fn the_block_load_bins_by_share_of_the_period() {
        use super::super::{LoadHistogram, LOAD_BINS};
        let load = LoadHistogram::default();
        let period = Duration::from_secs_f64(256.0 / 48_000.0);
        for _ in 0..998 {
            load.record(period.mul_f64(0.105), 256, 48_000);
        }
        load.record(period.mul_f64(0.455), 256, 48_000);
        load.record(period * 3, 256, 48_000);
        let all = load.snapshot();
        assert_eq!(all.count(), 1000);
        assert_eq!(all.quantile(0.5), Some(10));
        assert_eq!(all.quantile(0.999), Some(45));
        assert_eq!(all.max(), Some(LOAD_BINS - 1), "longer than the last bin lands in it");
        assert_eq!(all.since(&all).quantile(0.5), None, "an empty phase has no quantile");
    }

    #[test]
    fn the_latency_is_the_running_median_then_frozen() {
        let mut probe = Probe::new();
        let ms = |ms: u64| Some(Duration::from_millis(ms));
        assert_eq!(probe.observe(ms(10), 48_000), Some(480));
        assert_eq!(probe.observe(ms(30), 48_000), Some(960));
        assert_eq!(probe.observe(None, 48_000), None, "no report");
        assert_eq!(probe.observe(ms(2_000), 48_000), None, "not a latency");
        assert_eq!(probe.observe(ms(20), 48_000), Some(960));
        for _ in 3..LATENCY_SAMPLES {
            probe.observe(ms(20), 48_000);
        }
        assert_eq!(probe.observe(ms(50), 48_000), None, "frozen after the first reports");
    }
}
