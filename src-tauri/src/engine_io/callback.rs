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
use lf_engine::{ProcessContext, SLOT_COUNT};
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
/// Frames the WASAPI input callback converts at a time.
const SCRATCH_FRAMES: usize = 1024;

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
    /// The capture channel each slot reads (`EngineHost::set_slot_input_channel`), 32 bits a slot in one
    /// word: a change of both is one store, so no callback reads half of it.
    picks: AtomicU64,
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
    /// Frames in the latest output callback: the size the device delivers, which the status reports on
    /// ASIO (a driver may run another size than the one asked for).
    pub(crate) block: AtomicU32,
}

impl Run {
    pub(crate) fn new(channels: [u32; SLOT_COUNT]) -> Run {
        Run {
            picks: AtomicU64::new(pack(channels)),
            fade_out: AtomicBool::new(false),
            faded: AtomicBool::new(false),
            fault: AtomicU8::new(0),
            error: Mutex::new(None),
            xrun: AtomicBool::new(false),
            in_latency: AtomicI64::new(0),
            out_latency: AtomicI64::new(0),
            callbacks: AtomicU64::new(0),
            block: AtomicU32::new(0),
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

    /// Each slot's capture channel, as the owner last set them.
    pub(crate) fn slot_channels(&self) -> [u32; SLOT_COUNT] {
        unpack(self.picks.load(Relaxed))
    }

    /// Set every slot's capture channel at once (the owner; the callbacks read them each block).
    pub(crate) fn set_slot_channels(&self, channels: [u32; SLOT_COUNT]) {
        self.picks.store(pack(channels), Relaxed);
    }

    /// Each slot's capture channel on an input with `channels` channels (a pick past them reads the last).
    fn picks(&self, channels: usize) -> [usize; SLOT_COUNT] {
        self.slot_channels().map(|c| (c as usize).min(channels - 1))
    }

    /// The alignment the callbacks measured so far: (input + output, input), in frames.
    pub(crate) fn latency(&self) -> (Frame, Frame) {
        let (input, output) = (self.in_latency.load(Relaxed), self.out_latency.load(Relaxed));
        (input + output, input)
    }
}

const _: () = assert!(SLOT_COUNT * 32 <= 64, "every slot's pick fits the one word");

/// Every slot's capture channel in one word, slot `s` in bits `32 s..32 (s + 1)`.
fn pack(channels: [u32; SLOT_COUNT]) -> u64 {
    channels.iter().enumerate().fold(0, |word, (s, &c)| word | (c as u64) << (32 * s))
}

fn unpack(word: u64) -> [u32; SLOT_COUNT] {
    std::array::from_fn(|s| (word >> (32 * s)) as u32)
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
/// copies each slot's channel into its handoff for this cycle's output callback and counts the cycle.
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
        let (channels, picks) = (self.channels, self.run.picks(self.channels));
        let rt = &mut *rt;
        let ok = guarded(&self.core.counters, || {
            let n = data.len() / channels;
            for (handoff, channel) in rt.handoff.iter_mut().zip(picks) {
                for (h, frame) in handoff.iter_mut().zip(data.chunks_exact(channels)) {
                    *h = f32::from_sample(frame[channel]);
                }
            }
            rt.handoff_len = n;
            rt.in_cycles += 1;
        });
        if !ok {
            self.core.latch_fault(rt);
        }
    }
}

/// WASAPI's input callback, on its own thread and clock: pushes each slot's channel, interleaved, into
/// the join pipe the output callback pulls from (drop-on-full, counted), and measures the capture's age.
pub(crate) struct JoinInput {
    core: Arc<Core>,
    run: Arc<Run>,
    channels: usize,
    /// The engine's rate: the age is measured in its frames.
    rate: u32,
    push: PushEnd,
    /// `SCRATCH_FRAMES` frames of the slots' streams, interleaved.
    scratch: Vec<f32>,
    probe: Probe,
    /// A caught panic: the input stays silent for the rest of the run.
    dead: bool,
}

impl JoinInput {
    /// Allocates: build it on the owner thread.
    pub(crate) fn new(core: Arc<Core>, run: Arc<Run>, channels: usize, rate: u32, push: PushEnd) -> JoinInput {
        JoinInput { core, run, channels: channels.max(1), rate, push, scratch: vec![0.0; SCRATCH_FRAMES * SLOT_COUNT], probe: Probe::new(), dead: false }
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
        let (channels, picks) = (self.channels, self.run.picks(self.channels));
        let (push, scratch, counters) = (&mut self.push, &mut self.scratch, &self.core.counters);
        let ok = guarded(counters, || {
            let mut dropped = 0;
            for chunk in data.chunks(channels * SCRATCH_FRAMES) {
                let out = &mut scratch[..chunk.len() / channels * SLOT_COUNT];
                for (o, frame) in out.chunks_exact_mut(SLOT_COUNT).zip(chunk.chunks_exact(channels)) {
                    for (s, &channel) in o.iter_mut().zip(&picks) {
                        *s = f32::from_sample(frame[channel]);
                    }
                }
                dropped += push.push(out);
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
    /// ASIO: this cycle's handoffs in `Rt`.
    Duplex,
    /// WASAPI: the join pipe, pulled to exactly this block (the slots' streams interleaved in `x`), then
    /// each slot's into `xs`. `joined` once a pull came back whole: the startup pulls before the input
    /// arrives are not starves.
    Join { pipe: PullPipe, x: Vec<f32>, xs: [Vec<f32>; SLOT_COUNT], joined: bool, delay: Option<Frame> },
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
    /// WASAPI: the join spliced the previous block's input (a starve, a trim, an overrun's seam), and
    /// the resampler carries a few of those frames into this one: its input is damaged too.
    spliced: bool,
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
        let x = vec![0.0; MAX_DEVICE_BLOCK * SLOT_COUNT];
        let xs = std::array::from_fn(|_| vec![0.0; MAX_DEVICE_BLOCK]);
        Render::new(core, run, channels, rate, Source::Join { pipe, x, xs, joined: false, delay: None })
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
            spliced: false,
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
        self.run.block.store(n as u32, Relaxed);

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
        // A starve or a trim after the startup priming splices the input, and a pull may hold an
        // overrun's seam until it is surely played (`PullPipe::take_seam`): the block's input is damaged.
        let mut spliced = false;
        let avail = match &mut self.source {
            Source::Duplex => 0,
            Source::Join { pipe, x, xs, joined, .. } => {
                let m = n.min(MAX_DEVICE_BLOCK);
                // The input captured while the buffer played dry belongs to the frames skipped below.
                pipe.skip(lost as usize);
                #[cfg(debug_assertions)]
                let fill_before = pipe.fill();
                let zeroed = pipe.pull(&mut x[..m * SLOT_COUNT]) + (n - m);
                for (s, xs) in xs.iter_mut().enumerate() {
                    for (y, frame) in xs[..m].iter_mut().zip(x.chunks_exact(SLOT_COUNT)) {
                        *y = frame[s];
                    }
                }
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
                    spliced = true;
                }
                spliced |= (trims > 0) | pipe.take_seam();
                m
            }
        };
        let frame = core.frame.load(Relaxed) + lost;
        let xrun = (lost > 0) | std::mem::take(&mut self.missed) | self.run.xrun.swap(false, Acquire);
        let damaged = spliced | std::mem::replace(&mut self.spliced, spliced);
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
                let ctx = ProcessContext { frame, xrun, damaged, align_frames: align, input_frames };
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
        let mut damaged = ctx.damaged;
        let avail = match self.source {
            Source::Duplex => {
                // A run's first output callback takes the input's count: the input starts playing just
                // before the output, so the cycles it ran alone are the start, not a fault.
                rt.out_cycles = if rt.out_cycles == 0 { rt.in_cycles } else { rt.out_cycles + 1 };
                let same = rt.in_cycles == rt.out_cycles && rt.handoff_len == n && n <= MAX_DEVICE_BLOCK;
                if !same && rt.out_cycles > 0 {
                    // Out of step: count it once and resync, as the Stage 1 spike does. The block renders
                    // from silence: its input is damaged.
                    counters.duplex_faults.fetch_add(1, Relaxed);
                    rt.out_cycles = rt.in_cycles;
                    damaged = true;
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
        let inputs: [&[f32]; SLOT_COUNT] = match &self.source {
            Source::Duplex => handoff.each_ref().map(|h| &h[..]),
            Source::Join { xs, .. } => xs.each_ref().map(|x| &x[..]),
        };
        for input in inputs {
            meter(&self.core, &input[..avail.min(input.len())]);
        }
        let mut engine = engine.as_mut().filter(|_| !*faulted);
        let max_block = self.left.len();
        let target = if fading { 0.0 } else { 1.0 };
        let mut off = 0;
        while off < n {
            let m = (n - off).min(max_block);
            let x = inputs.map(|input| if off + m <= avail { &input[off..off + m] } else { &self.zeros[..m] });
            let (left, right) = (&mut self.left[..m], &mut self.right[..m]);
            match engine.as_deref_mut() {
                Some(engine) => {
                    let ctx = ProcessContext { frame: ctx.frame + off as Frame, xrun: ctx.xrun && off == 0, damaged, ..ctx };
                    engine.process_inputs(&ctx, x, left, right);
                }
                None => {
                    left.fill(0.0);
                    right.fill(0.0);
                }
            }
            fade(&mut self.gain, self.step, target, left, right);
            #[cfg(debug_assertions)]
            if let Some(lag) = lag.as_mut() {
                lag.block(ctx.frame + off as Frame, x[0], left, right);
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
    fn every_slots_pick_travels_in_one_word() {
        let run = Run::new([3, u32::MAX]);
        assert_eq!(run.slot_channels(), [3, u32::MAX]);
        run.set_slot_channels([u32::MAX, 0]);
        assert_eq!(run.slot_channels(), [u32::MAX, 0], "a swap lands whole");
        assert_eq!(run.picks(2), [1, 0], "a pick past the device's channels reads its last");
    }

    const RATE: u32 = 48_000;
    /// A WASAPI period at `RATE`: 10 ms.
    const N: usize = 480;

    /// The input's frame code: every frame its own value, never silence.
    fn code(frame: Frame) -> f32 {
        0.25 + (frame % 4096) as f32 / 16384.0
    }

    /// WASAPI's join by hand, on synthetic time: [`JoinInput`] pushes 10 ms periods of [`code`] and
    /// [`Render`] pulls 10 ms blocks through the join pipe into an engine.
    struct Join {
        core: Arc<Core>,
        input: JoinInput,
        render: Render,
        handle: lf_engine::EngineHandle,
        pushed: Frame,
        entry: Instant,
        events: Vec<lf_engine::Event>,
    }

    /// The device side's shared state with an engine at `RATE` in its lock, and the engine's handle.
    fn engine_core() -> (Arc<Core>, lf_engine::EngineHandle) {
        let core = Arc::new(Core::new());
        let config = lf_engine::EngineConfig { max_loop_seconds: 4.0, ..lf_engine::EngineConfig::new(RATE) };
        let (engine, handle) = lf_engine::Engine::new(config);
        core.rt.lock().unwrap().engine = Some(engine);
        core.rate.store(RATE, Relaxed);
        core.max_block.store(config.max_block as u32, Relaxed);
        (core, handle)
    }

    impl Join {
        fn new() -> Join {
            use super::super::pipes::{pipe, PipeConfig};
            let (core, handle) = engine_core();
            let run = Arc::new(Run::new([0, 0]));
            // The device owner's join (`owner.rs`): 500 ms of ring, a 25 ms setpoint.
            let join = PipeConfig { in_rate: RATE, out_rate: RATE, channels: SLOT_COUNT, capacity: RATE as usize / 2, setpoint: 0.025, max_pull: MAX_DEVICE_BLOCK };
            let (push, pull) = pipe(join).unwrap();
            let input = JoinInput::new(core.clone(), run.clone(), 1, RATE, push);
            let render = Render::join(core.clone(), run, 2, RATE, pull);
            Join { core, input, render, handle, pushed: 0, entry: Instant::now(), events: Vec::new() }
        }

        /// `periods` input periods at once, then one output block, on time.
        fn cycle(&mut self, periods: usize) {
            for _ in 0..periods {
                let data: Vec<f32> = (0..N as Frame).map(|k| code(self.pushed + k)).collect();
                self.input.capture(&data, None);
                self.pushed += N as Frame;
            }
            let mut out = [0.0f32; 2 * N];
            self.render.render(&mut out, self.entry, None);
            self.entry += Duration::from_millis(10);
            while let Ok(e) = self.handle.events.pop() {
                self.events.push(e);
            }
        }

        fn send(&mut self, frame: Frame, command: lf_engine::Command) {
            self.handle.commands.push(lf_engine::TimedCommand { frame: Some(frame), command }).expect("command ring full");
        }

        fn frame(&self) -> Frame {
            self.core.frame.load(Relaxed)
        }

        fn engine<R>(&self, f: impl FnOnce(&lf_engine::Engine) -> R) -> R {
            f(self.core.rt.lock().unwrap().engine.as_ref().unwrap())
        }

        /// On time up to frame `trim`, then the output stalls while the input runs on (half a second of
        /// input arrives at once): the ring fills, the input past it is dropped, and the next pull trims
        /// the ring back to its 25 ms. Its last 1200 frames before the drop play over that block and the
        /// next, and the seam falls 240 frames into the third.
        fn overrun_at(&mut self, trim: Frame) {
            while self.frame() < trim {
                self.cycle(1);
            }
            assert_eq!(self.frame(), trim);
            self.cycle(50);
            assert!(self.core.counters.join_overruns.load(Relaxed) > 0, "the ring ran over");
            assert_eq!(self.core.counters.join_trims.load(Relaxed), 1);
        }
    }

    #[test]
    fn a_join_overrun_rejects_the_take_its_seam_falls_in() {
        use lf_engine::{Command, Event};
        let mut j = Join::new();
        for _ in 0..20 {
            j.cycle(1);
        }
        let now = j.frame();
        for command in [Command::SetBpm(120.0), Command::SetSlotLive(0, true), Command::SetFixedLength(true), Command::SetFixedBars(1.0)] {
            j.send(now, command);
        }
        j.cycle(1);
        // A first take of one bar: its window opens a bar of count-in (4 x 24000 frames) plus the
        // alignment after the press. Press so that it opens 100 frames into a block.
        let k = 4 * 24_000 + j.core.align_frames.load(Relaxed) + j.engine(|e| e.limiter_latency());
        let press = j.frame() + 480 + (100 - k).rem_euclid(N as Frame);
        j.send(press, Command::RecDub(0));
        j.cycle(1);
        j.cycle(1);
        let start = press + k;
        assert_eq!(j.engine(|e| e.looper().recorder()), Some((0, Some(start), Some(start + 96_000))), "the window");
        // The take opens 100 frames into the block the seam falls in, before the seam.
        j.overrun_at(start - 100 - 2 * N as Frame);
        while j.frame() < start + 96_000 + 4 * N as Frame {
            j.cycle(1);
        }
        assert!(j.events.iter().any(|e| matches!(e, Event::TakeRejected { lane: 0, overdub: false, .. })), "the spliced take is rejected");
    }

    #[test]
    fn a_join_overrun_rejects_the_layer_its_seam_falls_in_and_keeps_the_loop() {
        use lf_engine::{Command, Event};
        let mut j = Join::new();
        for _ in 0..20 {
            j.cycle(1);
        }
        let now = j.frame();
        for command in [Command::SetBpm(120.0), Command::SetSlotLive(0, true), Command::SetFixedLength(true), Command::SetFixedBars(1.0), Command::SetDubFeedback(0, 0.5)] {
            j.send(now, command);
        }
        j.cycle(1);
        let now = j.frame();
        j.send(now, Command::RecDub(0));
        while j.engine(|e| e.looper().master() == 0 || e.looper().recorder().is_some()) {
            j.cycle(1);
        }
        let before = j.engine(|e| e.looper().loop_pcm(0));
        assert!(before.iter().all(|&x| x != 0.0), "a clean one-bar loop");
        // The layer opens 100 frames into the block the seam falls in, after the trimmed pull's two
        // blocks, and is armed well before them.
        let k = j.core.align_frames.load(Relaxed) + j.engine(|e| e.limiter_latency());
        let start = j.frame() + 8 * N as Frame + 100;
        let trim = start - 100 - 2 * N as Frame;
        assert!(start - k < trim, "the press lands before the stall");
        j.send(start - k, Command::RecDub(0));
        while j.frame() < trim {
            j.cycle(1);
        }
        assert_eq!(j.engine(|e| e.looper().recorder()), Some((0, Some(start), None)), "the layer's window");
        j.overrun_at(trim);
        while j.frame() < start + 48_000 {
            j.cycle(1);
        }
        let now = j.frame();
        j.send(now, Command::RecDub(0));
        while j.engine(|e| e.looper().recorder().is_some() || e.looper().busy()) {
            j.cycle(1);
        }
        assert!(j.events.iter().any(|e| matches!(e, Event::TakeRejected { lane: 0, overdub: true, .. })), "the spliced layer is rejected");
        assert!(j.engine(|e| e.looper().loop_pcm(0)) == before, "the loop before the layer, bit for bit");
    }

    #[test]
    fn a_lock_held_across_cycles_is_one_input_gap_whichever_input_it_costs() {
        // ASIO by hand: the engine lock taken just before or after a cycle's input, and given back just
        // before or after another's. The first output that takes it again follows the misses, and when
        // its input is out of step it is a duplex fault too: one block, one input gap.
        const B: usize = 256;
        for (after_input, before_input) in [(false, false), (false, true), (true, false), (true, true)] {
            let (core, _handle) = engine_core();
            let run = Arc::new(Run::new([0, 0]));
            let mut input = DuplexInput::new(core.clone(), run.clone(), 1, RATE);
            let mut render = Render::duplex(core.clone(), run, 2, RATE);
            let x = [0.25f32; B];
            let mut out = [0.0f32; 2 * B];
            let mut output = |render: &mut Render| render.render(&mut out, Instant::now(), None);
            for _ in 0..4 {
                input.capture(&x, None);
                output(&mut render);
            }
            if after_input {
                input.capture(&x, None);
            }
            let held = core.rt.lock().unwrap();
            if !after_input {
                input.capture(&x, None);
            }
            output(&mut render);
            for _ in 0..3 {
                input.capture(&x, None);
                output(&mut render);
            }
            if !before_input {
                input.capture(&x, None);
            }
            drop(held);
            if before_input {
                input.capture(&x, None);
            }
            output(&mut render);
            for _ in 0..4 {
                input.capture(&x, None);
                output(&mut render);
            }
            let case = format!("taken {} an input, given back {} one", if after_input { "after" } else { "before" }, if before_input { "before" } else { "after" });
            let counters = &core.counters;
            assert_eq!(counters.lock_misses.load(Relaxed), 7 + u64::from(!after_input) + u64::from(!before_input), "{case}");
            let faults = counters.duplex_faults.load(Relaxed);
            assert_eq!(faults, u64::from(after_input == before_input), "{case}: the resumed output's input is out of step");
            let xruns = core.rt.lock().unwrap().engine.as_ref().unwrap().diag().xruns;
            assert_eq!(xruns, 1, "{case}: one input gap");
        }
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
