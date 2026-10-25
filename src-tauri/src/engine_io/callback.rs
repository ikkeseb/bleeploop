//! OWNS: the device callbacks' bodies, shared by ASIO, WASAPI and the fake driver: the output callback
//! that renders the engine on the one clock ([`Render`]), the input callbacks that feed it ([`Capture`]:
//! ASIO's same-cycle handoff, WASAPI's join pipe), and what one run's callbacks share with the owner
//! ([`Run`]). A run is one pair of streams, from their start to their drop.
//!
//! Each body takes plain values (the device's samples, the instant the callback entered, the latency
//! the driver reported for it), so the fake driver runs them on synthetic time. The engine lock is only
//! `try_lock`ed, and everything under it runs in [`guarded`]: a panic is caught inside the guard, so the
//! lock is never poisoned and nothing unwinds into the driver (asio-sys's `extern "C"` bufferSwitch
//! would abort). Each callback runs whole inside an [`RtSection`]: the panic hook neither logs nor
//! chains there, and DEV builds count its allocations.

use std::cell::Cell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicU8, Ordering::{Acquire, Relaxed, Release}};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use cpal::{FromSample, Sample, SizedSample};
use lf_engine::grid::Frame;
use lf_engine::{ProcessContext, SLOT_COUNT};
use rtrb::{Consumer, Producer};

use super::fpu::DenormalsOff;
use super::pipes::{PullPipe, PushEnd};
use super::{Core, IoCounters, Rt};

/// The largest device callback the bodies take whole, in frames (the ASIO handoff, the join's pull
/// buffer). The excess of a larger one renders from silence: an input gap, never out of bounds.
pub(crate) const MAX_DEVICE_BLOCK: usize = 8192;
/// The fade on a switch, out and in: a declick ramp on the output.
const FADE_SECONDS: f64 = 0.010;
/// Silent callbacks after a fade-out before the owner may drop the streams: a dropped ASIO stream only
/// loses its callback, and the driver keeps playing its two buffer halves.
const SILENT_BLOCKS: u32 = 2;
/// Latency reports whose median the alignment takes before it freezes for the run.
pub(crate) const LATENCY_SAMPLES: usize = 16;
/// An input sample format the callbacks take, and its full scale (the meter's clip): either rail of the
/// device's own format, so a sample one step under one is no clip. The input callbacks read it before
/// the conversion, which rounds an I32's top codes to 1.0 (and the join's resampler moves a float). A
/// converter narrower than its container (24 bits in an I32, any in an F32) tops out a step under the
/// positive rail, so it lights CLIP on the negative one alone, which every two's-complement one reaches.
pub(crate) trait Rail: SizedSample {
    fn full_scale(self) -> bool;
}

impl Rail for f32 {
    fn full_scale(self) -> bool {
        self >= 1.0 || self <= -1.0
    }
}

macro_rules! integer_rails {
    ($($T:ty),*) => {$(
        impl Rail for $T {
            fn full_scale(self) -> bool {
                self == <$T>::MAX || self == <$T>::MIN
            }
        }
    )*};
}
integer_rails!(i32, i16, u16);

/// Light the clip of each slot whose input reached full scale in a callback.
fn latch_clips(core: &Core, clips: [bool; SLOT_COUNT]) {
    for (latch, clip) in core.meter_clip.iter().zip(clips) {
        if clip {
            latch.store(true, Relaxed);
        }
    }
}
/// Frames the WASAPI input callback converts at a time.
const SCRATCH_FRAMES: usize = 1024;
/// ASIO phase slips ([`PhaseSlips`]): the span of each window, and of a run's start that is no
/// baseline, in seconds of delivered frames.
const SLIP_WINDOW_SECONDS: f64 = 0.5;
/// ASIO output callbacks that finish this many periods or more after the wakes' best phase (their
/// entry's lag plus their own duration) count in `asio_late_finishes` ([`finishes_late`]), once per
/// episode: a new one needs [`LATE_FINISH_QUIET`] callbacks under it first. An empirical indicator from
/// the rig's loopback tone (src-tauri/OPEN-THREADS.md, the crackle thread), not proof: at 64 frames every break
/// had one at 2.52 or more and none away from a break passed 2.47, so near misses count too (4 in a
/// 10-minute soak beside a build that broke nothing); one break at 256 finished at 2.36 and would not
/// count. A count is not a heard break, and none is not a clean sound.
pub(crate) const LATE_FINISH_PERIODS: f64 = 2.4;
/// Callbacks under [`LATE_FINISH_PERIODS`] that end an episode: 46 ms at 64 frames and 44.1 kHz, longer
/// than the clusters a break came with, so a stall or a lasting slip whose callbacks hover at the edge
/// counts once.
const LATE_FINISH_QUIET: u32 = 32;

/// Share output's end in the callback (`share::ShareTap`; a fake in tests): fed the engine's Share mix
/// every block (`Engine::share`: the master without the click), faded as the device output is. Never
/// blocks or allocates.
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

/// cpal 0.18.1's stream errors that end the stream: every one but an xrun, a default-device change (Windows
/// moved or removed its default; a stream on the default moves with it: `transition::follows`) and a
/// refused thread priority, which it documents as non-fatal. The
/// engine's streams (`Run::stream_error`) and Share output's mirror (`share.rs`) both go by it.
pub(crate) fn ends_stream(error: &cpal::Error) -> bool {
    !matches!(error.kind(), cpal::ErrorKind::Xrun | cpal::ErrorKind::DeviceChanged | cpal::ErrorKind::RealtimeDenied)
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
    /// WASAPI: the input callback caught a panic and went silent, its stream still running: the owner
    /// rebuilds the run on the same device (`Owner::recapture`).
    pub(crate) capture_fault: AtomicBool,
    /// cpal reported an xrun: the next block follows an input gap.
    xrun: AtomicBool,
    /// cpal reported that Windows moved its default endpoint: the owner reads where a default side plays now.
    pub(crate) default_moved: AtomicBool,
    /// ASIO: cpal hands each overload to both streams' error callbacks, but its `try_emit_error` can drop
    /// either under contention. Each side's reports this run, and the overloads counted from them: the
    /// larger side, so an overload both sides heard counts once and a lone report still counts (two
    /// overloads each heard by a different side alone count once).
    duplex: AtomicBool,
    overloads: [AtomicU64; 2],
    overloads_counted: AtomicU64,
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
            capture_fault: AtomicBool::new(false),
            xrun: AtomicBool::new(false),
            default_moved: AtomicBool::new(false),
            duplex: AtomicBool::new(false),
            overloads: [AtomicU64::new(0), AtomicU64::new(0)],
            overloads_counted: AtomicU64::new(0),
            in_latency: AtomicI64::new(0),
            out_latency: AtomicI64::new(0),
            callbacks: AtomicU64::new(0),
            block: AtomicU32::new(0),
        }
    }

    /// A stream's error callback. An xrun is counted (on ASIO, by the side that heard more) and flags the next
    /// block, whichever side reported it. cpal 0.18.1 documents a
    /// default-device change (latched for the owner) and a refused thread priority as
    /// non-fatal; any other error ends the stream: latched for the owner, which drops the run and falls
    /// back. Never blocks (an xrun can arrive on the driver's thread).
    pub(crate) fn stream_error(&self, side: Side, error: cpal::Error, counters: &IoCounters) {
        match error.kind() {
            cpal::ErrorKind::Xrun => {
                if self.duplex.load(Relaxed) {
                    let [input, output] = &self.overloads;
                    let (mine, other) = match side {
                        Side::Input => (input, output),
                        Side::Output => (output, input),
                    };
                    // Each report sees its own side's exact count, so the last one leaves the larger side
                    // counted, whichever way the two race.
                    let seen = (mine.fetch_add(1, Relaxed) + 1).max(other.load(Relaxed));
                    let counted = self.overloads_counted.fetch_max(seen, Relaxed);
                    counters.xruns.fetch_add(seen.saturating_sub(counted), Relaxed);
                } else {
                    counters.xruns.fetch_add(1, Relaxed);
                }
                self.xrun.store(true, Release);
            }
            cpal::ErrorKind::DeviceChanged => self.default_moved.store(true, Release),
            _ if !ends_stream(&error) => {}
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

/// ASIO: output callbacks whose phase moved later for good. asio-sys drops the driver's sample position
/// and the rig's driver's system time is not monotonic, so the only clock is the host's at callback
/// entry. A wake's offset is the time since the first wake, in frames, less the frames delivered since;
/// a window's floor is its smallest offset (windows counted in delivered frames, so the fake's synthetic
/// time works too). A single wake says nothing: the rig's USB driver wakes on a ~1 ms cadence, so at 64
/// frames its wakes spread over more than a period with every fault counter 0. A floor half a period or
/// more above the window before's is a slip: a window holds hundreds of on-time wakes, so jitter and
/// a late wake the driver makes up leave its floor in place, and a drifting clock moves it a few frames.
/// Windows that rise in a row are one slip: a move inside a window can lift its floor only partly.
/// A slip cannot tell a dropped period from a lasting move of the driver's phase, nor from a stall the
/// driver makes up only over more than a window; one wake half a period earlier than the rest would
/// read as one too (unmeasured), and two moves in consecutive windows as one. A falling floor counts
/// nothing. A new block size starts over, and a move in its first two windows (~1 s) counts nothing.
struct PhaseSlips {
    /// The first wake at this block size, and the frames delivered since.
    first: Option<Instant>,
    block: usize,
    delivered: u64,
    /// The smallest offset in this window and in the one before, the frames in this one, and the
    /// windows closed at this block size.
    floor: f64,
    floor_before: f64,
    window: u64,
    windows: u64,
    /// The window before rose: a rise now continues its slip.
    rising: bool,
}

impl PhaseSlips {
    const fn new() -> PhaseSlips {
        PhaseSlips { first: None, block: 0, delivered: 0, floor: f64::INFINITY, floor_before: f64::INFINITY, window: 0, windows: 0, rising: false }
    }

    /// One output callback of `n` frames entered at `entry`: how far behind the floor it came, in
    /// frames (0 or more; [`late_past_period`] keeps what is past a period), and whether it closed a
    /// window that started a slip. `None` through the start.
    fn wake(&mut self, entry: Instant, n: usize, rate: u32) -> Option<(f64, bool)> {
        if n == 0 || rate == 0 {
            return None;
        }
        let first = match self.first {
            Some(first) if self.block == n => first,
            _ => {
                *self = PhaseSlips { first: Some(entry), block: n, ..PhaseSlips::new() };
                entry
            }
        };
        let offset = entry.saturating_duration_since(first).as_secs_f64() * rate as f64 - self.delivered as f64;
        self.floor = self.floor.min(offset);
        let behind = offset - self.floor.min(self.floor_before);
        let started = self.windows > 0;
        self.delivered += n as u64;
        self.window += n as u64;
        let mut slipped = false;
        if self.window >= (SLIP_WINDOW_SECONDS * rate as f64) as u64 {
            // From the third window on: the start's floor is no baseline.
            let rose = self.windows > 1 && self.floor - self.floor_before >= n as f64 / 2.0;
            slipped = rose && !self.rising;
            self.rising = rose;
            self.floor_before = std::mem::replace(&mut self.floor, f64::INFINITY);
            self.window = 0;
            self.windows += 1;
        }
        started.then_some((behind, slipped))
    }
}

/// An entry `behind` frames behind the floor, in whole frames when that is more than a period of `n`
/// (else 0: the rig's jitter alone reaches 92 to 109 frames at 64, so `asio_late_max` logs a rise past
/// that).
fn late_past_period(behind: f64, n: usize) -> u64 {
    if behind > n as f64 { behind.round() as u64 } else { 0 }
}

/// A callback of `n` frames that entered `behind` frames behind the best phase and took `took` frames
/// finished [`LATE_FINISH_PERIODS`] periods or more after it.
pub(crate) fn finishes_late(behind: f64, took: f64, n: usize) -> bool {
    behind + took >= LATE_FINISH_PERIODS * n as f64
}

thread_local! {
    static PROMOTED: Cell<bool> = const { Cell::new(false) };
    /// This thread is inside an audio callback ([`RtSection`]). Const-initialised, so the panic hook
    /// reads it without allocating.
    static IN_CALLBACK: Cell<bool> = const { Cell::new(false) };
}

/// Whether this thread is inside an audio callback: the panic hook then neither logs nor chains (both
/// allocate, and the log takes locks), and the callback's own panic counter reports it owner-side.
pub(crate) fn in_rt_callback() -> bool {
    IN_CALLBACK.with(Cell::get)
}

/// One audio callback, whole (input, output, Share output's), from its entry to its return: marks the
/// thread for the panic hook and, in DEV builds, counts the allocations made under it into
/// `rt_allocs` (`host::rt_alloc`, this thread's). A section entered inside another leaves both to the
/// outer one, so nothing counts twice.
pub(crate) struct RtSection<'a> {
    /// `None` inside another section.
    counters: Option<&'a IoCounters>,
    #[cfg(debug_assertions)]
    before: u64,
    #[cfg(debug_assertions)]
    alloc: Option<crate::host::rt_alloc::Guard>,
}

impl<'a> RtSection<'a> {
    pub(crate) fn enter(counters: &'a IoCounters) -> RtSection<'a> {
        let outer = IN_CALLBACK.with(|c| c.replace(true));
        RtSection {
            counters: (!outer).then_some(counters),
            #[cfg(debug_assertions)]
            before: crate::host::rt_alloc::allocations(),
            #[cfg(debug_assertions)]
            alloc: (!outer).then(crate::host::rt_alloc::guard),
        }
    }
}

impl Drop for RtSection<'_> {
    fn drop(&mut self) {
        let Some(counters) = self.counters else { return };
        #[cfg(debug_assertions)]
        {
            drop(self.alloc.take());
            let after = crate::host::rt_alloc::allocations();
            if after > self.before {
                counters.rt_allocs.fetch_add(after - self.before, Relaxed);
            }
        }
        #[cfg(not(debug_assertions))]
        let _ = counters;
        IN_CALLBACK.with(|c| c.set(false));
    }
}

/// Test-only: an allocation at a named spot of a callback, asked for by a test on its own thread. The
/// RT allocation count's reach is checked with these: each sits where a callback once ran uncounted.
#[cfg(test)]
pub(crate) mod inject {
    use std::cell::Cell;

    thread_local! {
        static AT: Cell<&'static str> = const { Cell::new("") };
    }

    /// The next time this thread passes `site`, allocate once.
    pub(crate) fn alloc_at(site: &'static str) {
        AT.with(|at| at.set(site));
    }

    pub(crate) fn site(site: &'static str) {
        if AT.with(|at| at.get() == site) {
            AT.with(|at| at.set(""));
            drop(std::hint::black_box(vec![0u8; 16]));
        }
    }
}

/// MMCSS Pro Audio for the calling thread, once (every engine, join and share callback thread).
pub(crate) fn promote_once() {
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

/// Run `body` as a callback's guarded section: a panic is caught and counted (false). Its allocations
/// count with the rest of the callback's ([`RtSection`]).
pub(crate) fn guarded(counters: &IoCounters, body: impl FnOnce()) -> bool {
    let ok = catch_unwind(AssertUnwindSafe(body)).is_ok();
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
    pub(crate) fn capture<T: Rail>(&mut self, data: &[T], latency: Option<Duration>)
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

    fn capture<T: Rail>(&mut self, data: &[T], latency: Option<Duration>)
    where
        f32: FromSample<T>,
    {
        let section = Arc::clone(&self.core);
        let _rt = RtSection::enter(&section.counters);
        promote_once();
        let _ftz = DenormalsOff::new();
        if let Some(frames) = self.probe.observe(latency, self.rate) {
            self.run.in_latency.store(frames, Relaxed);
        }
        let Some(mut rt) = try_rt(&self.core) else {
            self.core.counters.lock_misses.fetch_add(1, Relaxed);
            return;
        };
        let (channels, picks) = (self.channels, self.run.picks(self.channels));
        let rt = &mut *rt;
        let core = &*self.core;
        let ok = guarded(&core.counters, || {
            let n = data.len() / channels;
            let mut clips = [false; SLOT_COUNT];
            for ((handoff, channel), clip) in rt.handoff.iter_mut().zip(picks).zip(&mut clips) {
                for (h, frame) in handoff.iter_mut().zip(data.chunks_exact(channels)) {
                    *clip |= frame[channel].full_scale();
                    *h = f32::from_sample(frame[channel]);
                }
            }
            latch_clips(core, clips);
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
/// A caught panic leaves its pipe end unusable: the input goes silent and latches `Run::capture_fault`
/// for the owner, which rebuilds the run (`Owner::recapture`).
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
    /// Test-only: the next callback panics inside its guard.
    #[cfg(test)]
    pub(crate) panic_next: bool,
}

impl JoinInput {
    /// Allocates: build it on the owner thread.
    pub(crate) fn new(core: Arc<Core>, run: Arc<Run>, channels: usize, rate: u32, push: PushEnd) -> JoinInput {
        JoinInput {
            core,
            run,
            channels: channels.max(1),
            rate,
            push,
            scratch: vec![0.0; SCRATCH_FRAMES * SLOT_COUNT],
            probe: Probe::new(),
            dead: false,
            #[cfg(test)]
            panic_next: false,
        }
    }

    fn capture<T: Rail>(&mut self, data: &[T], latency: Option<Duration>)
    where
        f32: FromSample<T>,
    {
        let section = Arc::clone(&self.core);
        let _rt = RtSection::enter(&section.counters);
        promote_once();
        let _ftz = DenormalsOff::new();
        #[cfg(test)]
        inject::site("join input");
        #[cfg(debug_assertions)]
        trace::join_push(data.len() / self.channels, latency);
        if let Some(frames) = self.probe.observe(latency, self.rate) {
            self.run.in_latency.store(frames, Relaxed);
        }
        if self.dead {
            return;
        }
        #[cfg(test)]
        let panic_now = std::mem::take(&mut self.panic_next);
        let (channels, picks) = (self.channels, self.run.picks(self.channels));
        let (push, scratch, core) = (&mut self.push, &mut self.scratch, &*self.core);
        let counters = &core.counters;
        let ok = guarded(counters, || {
            #[cfg(test)]
            if panic_now {
                panic!("the capture fails (on purpose)");
            }
            let (mut dropped, mut clips) = (0, [false; SLOT_COUNT]);
            for chunk in data.chunks(channels * SCRATCH_FRAMES) {
                let out = &mut scratch[..chunk.len() / channels * SLOT_COUNT];
                for (o, frame) in out.chunks_exact_mut(SLOT_COUNT).zip(chunk.chunks_exact(channels)) {
                    for ((s, &channel), clip) in o.iter_mut().zip(&picks).zip(&mut clips) {
                        *clip |= frame[channel].full_scale();
                        *s = f32::from_sample(frame[channel]);
                    }
                }
                dropped += push.push(out);
            }
            latch_clips(core, clips);
            if dropped > 0 {
                counters.join_overruns.fetch_add(1, Relaxed);
            }
        });
        if !ok {
            self.dead = true;
            self.run.capture_fault.store(true, Release);
        }
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
/// at most its `max_block`, writes stereo to the device, fades on a switch, feeds Share output's tap the
/// Share mix, publishes the frame clock and mirrors the engine's counters.
pub(crate) struct Render {
    core: Arc<Core>,
    run: Arc<Run>,
    channels: usize,
    rate: u32,
    source: Source,
    left: Vec<f32>,
    right: Vec<f32>,
    /// The Share mix of the slice, for the tap.
    share: [Vec<f32>; 2],
    zeros: Vec<f32>,
    /// The previous callback's entry and frames (WASAPI's dry buffer, the DEV trace).
    last: Option<(Instant, usize)>,
    /// ASIO: lasting slips of the wakes' phase (a diagnostic; the frame counter ignores them).
    slips: PhaseSlips,
    /// Callbacks in a row that did not finish late (`finishes_late`): an episode counts once.
    on_time: u32,
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
        run.duplex.store(true, Relaxed);
        Render::new(core, run, channels, rate, Source::Duplex)
    }

    /// WASAPI's output callback, pulling the input through the join pipe.
    pub(crate) fn join(core: Arc<Core>, run: Arc<Run>, channels: usize, rate: u32, pipe: PullPipe) -> Render {
        let x = vec![0.0; MAX_DEVICE_BLOCK * SLOT_COUNT];
        let xs = std::array::from_fn(|_| vec![0.0; MAX_DEVICE_BLOCK]);
        #[cfg(debug_assertions)]
        trace::join_start();
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
            share: [vec![0.0; max_block], vec![0.0; max_block]],
            zeros: vec![0.0; max_block],
            last: None,
            slips: PhaseSlips::new(),
            on_time: LATE_FINISH_QUIET,
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
        let core = Arc::clone(&self.core);
        let counters = &core.counters;
        let _rt = RtSection::enter(counters);
        // Its own clock, not `entry`: the fake driver's entries are synthetic.
        let began = Instant::now();
        // DEV: the thread's cycles at entry, read only in a tone run (`tone::cycle_stamp`).
        #[cfg(debug_assertions)]
        let began_cycles = self.core.tone.get().and_then(|_| super::tone::cycle_stamp());
        promote_once();
        let _ftz = DenormalsOff::new();
        counters.callbacks.fetch_add(1, Relaxed);
        self.run.callbacks.fetch_add(1, Relaxed);
        let n = data.len() / self.channels;
        self.run.block.store(n as u32, Relaxed);

        #[cfg(debug_assertions)]
        trace::output(self.last, entry, n, latency, self.rate, self.run.callbacks.load(Relaxed));
        // The frames the device played that this run never rendered. Only WASAPI can say (`dry_frames`).
        // ASIO's counter infers nothing from timing: each bufferSwitch carries one period in and out, so
        // it counts what the device took, and a late wake the driver makes up loses nothing (at 64
        // frames the rig's driver woke 98, 78 and 8 frames apart). A period the driver drops arrives
        // as its overload report, a cpal Xrun, when cpal delivers it; a lasting slip of the wakes' phase
        // is counted beside it, for the log only (`PhaseSlips`).
        self.cap = self.cap.max(n);
        let lost = match (self.last.replace((entry, n)), &self.source) {
            (Some((before, _)), Source::Join { .. }) => dry_frames(entry.saturating_duration_since(before), self.rate, n, self.cap),
            _ => 0,
        };
        // How far behind the wakes' best phase this one entered (ASIO, past the start), for its finish
        // below and the tone run's log.
        let mut entered_behind = None;
        if let (Source::Duplex, Some((behind, slipped))) = (&self.source, self.slips.wake(entry, n, self.rate)) {
            counters.asio_phase_slips.fetch_add(u64::from(slipped), Relaxed);
            counters.asio_late_max.fetch_max(late_past_period(behind, n), Relaxed);
            entered_behind = Some(behind);
        }
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
                #[cfg(test)]
                inject::site("join pull");
                let zeroed = pipe.pull(&mut x[..m * SLOT_COUNT]) + (n - m);
                for (s, xs) in xs.iter_mut().enumerate() {
                    for (y, frame) in xs[..m].iter_mut().zip(x.chunks_exact(SLOT_COUNT)) {
                        *y = frame[s];
                    }
                }
                let trims = pipe.take_trims();
                #[cfg(debug_assertions)]
                trace::join_pull(trace::Pull { callback: self.run.callbacks.load(Relaxed), entry, n, fill: fill_before, zeroed, trims, lost, latency, rate: self.rate, joined: *joined });
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
        let elapsed = began.elapsed();
        #[cfg(debug_assertions)]
        let end_cycles = began_cycles.and_then(|_| super::tone::cycle_stamp());
        counters.block_load.record(elapsed, n, self.rate);
        // Its entry's lag plus its own duration (cpal stamps the entry just before it), once an episode.
        if entered_behind.is_some_and(|behind| finishes_late(behind, elapsed.as_secs_f64() * self.rate as f64, n)) {
            if self.on_time >= LATE_FINISH_QUIET {
                counters.asio_late_finishes.fetch_add(1, Relaxed);
            }
            self.on_time = 0;
        } else {
            self.on_time = self.on_time.saturating_add(1);
        }
        #[cfg(debug_assertions)]
        if let Some(tone) = core.tone.get() {
            let cycles = began_cycles.zip(end_cycles).and_then(|((c0, t0), (c1, t1))| c1.checked_sub(c0).zip(t1.checked_sub(t0))).filter(|&(_, ticks)| ticks > 0);
            tone.slow.record(frame, n, elapsed, cycles, entered_behind.map_or(0, |b| b.round() as u64), self.rate);
        }
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
            #[cfg(debug_assertions)]
            tone,
            ..
        } = rt;
        let inputs: [&[f32]; SLOT_COUNT] = match &self.source {
            Source::Duplex => handoff.each_ref().map(|h| &h[..]),
            Source::Join { xs, .. } => xs.each_ref().map(|x| &x[..]),
        };
        for (slot, input) in inputs.iter().enumerate() {
            meter(&self.core, slot, &input[..avail.min(input.len())]);
        }
        let mut engine = engine.as_mut().filter(|_| !*faulted);
        let max_block = self.left.len();
        let target = if fading { 0.0 } else { 1.0 };
        let (mut off, mut clipped) = (0, false);
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
            // The Share mix fades frame for frame as the device output does: the same ramp from the same
            // gain.
            let share_gain = self.gain;
            fade(&mut self.gain, self.step, target, left, right);
            #[cfg(debug_assertions)]
            if let Some(lag) = lag.as_mut() {
                lag.block(ctx.frame + off as Frame, x[0], left, right);
            }
            // The tone reads slot 1's input: valid only when this slice's input reached the callback
            // (the same test that picks the zero fill above).
            #[cfg(debug_assertions)]
            if let Some(tone) = tone.as_mut() {
                tone.block(ctx.frame + off as Frame, x[1], off + m <= avail, left, right);
            }
            if let Some(tap) = tap.as_mut() {
                let [share_l, share_r] = &mut self.share;
                let (share_l, share_r) = (&mut share_l[..m], &mut share_r[..m]);
                match engine.as_deref() {
                    Some(engine) => {
                        let [l, r] = engine.share();
                        share_l.copy_from_slice(l);
                        share_r.copy_from_slice(r);
                    }
                    None => {
                        share_l.fill(0.0);
                        share_r.fill(0.0);
                    }
                }
                let mut gain = share_gain;
                fade(&mut gain, self.step, target, share_l, share_r);
                tap.push(share_l, share_r, counters);
            }
            clipped |= write(&mut data[off * self.channels..(off + m) * self.channels], self.channels, left, right);
            off += m;
        }
        if clipped {
            counters.clipped_blocks.fetch_add(1, Relaxed);
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

/// Fold a block of a slot's input into the meter's peak the feed takes. Its clip the input callbacks
/// latch, in the device's own format ([`Rail`]).
fn meter(core: &Core, slot: usize, input: &[f32]) {
    let peak = input.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
    if peak > 0.0 {
        // A non-negative f32's bits order as the value does.
        core.meter_peak[slot].fetch_max(peak.to_bits(), Relaxed);
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
/// callback (WASAPI: its buffer), its first `LATE_RECORDS` late wakes (more than 1.5 periods after the
/// previous) with the two callbacks after each (a late wake the device makes up, or a buffer run dry),
/// each join trim and starve with the fill it found, and every 1000th join output callback the frames the
/// input pushed and the output pulled so far (their rates, against QPC). A WASAPI run's first seconds
/// go to a buffer of their own, every push and every pull on one clock ([`join_lines`]).
#[cfg(debug_assertions)]
pub(crate) mod trace {
    use std::cell::Cell;
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering::{Acquire, Relaxed, Release}};
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    const LEN: usize = 1024;
    static RECORDS: [[AtomicI64; 6]; LEN] = [const { [const { AtomicI64::new(0) }; 6] }; LEN];
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    static IN_FRAMES: AtomicI64 = AtomicI64::new(0);
    static OUT_FRAMES: AtomicI64 = AtomicI64::new(0);
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    /// Late-wake records a run may still write: the rig's ASIO driver at 64 frames wakes 96, 15 and 81
    /// frames apart, every period "late", and would fill the buffer before the first WASAPI run.
    const LATE_RECORDS: i64 = 24;
    static LATE_LEFT: AtomicI64 = AtomicI64::new(LATE_RECORDS);

    /// A WASAPI run's pushes and pulls from its start, about ten seconds of 10 ms periods on both sides.
    /// A record's first field is its tag (the run's number and the record's kind), stored last: the
    /// probe reads while the callbacks write, and takes a record only once it is whole.
    const JOIN_LEN: usize = 2048;
    static JOIN: [[AtomicI64; 8]; JOIN_LEN] = [const { [const { AtomicI64::new(0) }; 8] }; JOIN_LEN];
    static JOIN_NEXT: AtomicUsize = AtomicUsize::new(0);
    static JOIN_RUN: AtomicI64 = AtomicI64::new(0);
    /// The run's start, in µs on `EPOCH`.
    static JOIN_FROM: AtomicI64 = AtomicI64::new(0);

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

    /// A push (`kind` 0) or a pull (1) of the run that plays.
    fn put_join(kind: i64, r: [i64; 7]) {
        let i = JOIN_NEXT.fetch_add(1, Relaxed);
        if i < JOIN_LEN {
            let [tag, fields @ ..] = &JOIN[i];
            for (a, v) in fields.iter().zip(r) {
                a.store(v, Relaxed);
            }
            tag.store(JOIN_RUN.load(Relaxed) * 2 + kind, Release);
        }
    }

    /// `at` in µs on the trace's clock.
    fn micros(at: Instant) -> i64 {
        at.saturating_duration_since(*EPOCH.get_or_init(Instant::now)).as_micros() as i64
    }

    /// An output callback of `n` frames, entered at `entry`, after `last` (the previous entry and its
    /// frames).
    pub(crate) fn output(last: Option<(Instant, usize)>, entry: Instant, n: usize, latency: Option<Duration>, rate: u32, callback: u64) {
        let (kind, elapsed, prev) = match last {
            None => {
                LATE_LEFT.store(LATE_RECORDS, Relaxed);
                (0, -1, 0)
            }
            Some((before, prev)) => {
                let e = entry.saturating_duration_since(before).as_secs_f64() * rate as f64;
                let kind = if prev > 0 && e > 1.5 * prev as f64 {
                    AFTER.set(2);
                    1
                } else if AFTER.get() > 0 {
                    AFTER.set(AFTER.get() - 1);
                    2
                } else {
                    return;
                };
                if LATE_LEFT.fetch_sub(1, Relaxed) <= 0 {
                    return;
                }
                (kind, e.round() as i64, prev)
            }
        };
        let latency = latency.map_or(-1, |d| (d.as_secs_f64() * rate as f64).round() as i64);
        put([kind, callback as i64, elapsed, prev as i64, n as i64, latency]);
    }

    /// A WASAPI run is wired (the owner thread, before either stream plays): its pushes and pulls start
    /// the join buffer over.
    pub(crate) fn join_start() {
        JOIN_FROM.store(micros(Instant::now()), Relaxed);
        JOIN_RUN.fetch_add(1, Relaxed);
        JOIN_NEXT.store(0, Relaxed);
    }

    /// One join output callback: `n` frames entered at `entry`, the `fill` its pull found (frames at the
    /// input's rate, after the `lost` frames' skip), the frames it zero-filled, its trims, the playback
    /// delay the stream reported, and whether a pull of this run has come back whole (`Source::Join`).
    pub(crate) struct Pull {
        pub(crate) callback: u64,
        pub(crate) entry: Instant,
        pub(crate) n: usize,
        pub(crate) fill: usize,
        pub(crate) zeroed: usize,
        pub(crate) trims: u64,
        pub(crate) lost: i64,
        pub(crate) latency: Option<Duration>,
        pub(crate) rate: u32,
        pub(crate) joined: bool,
    }

    pub(crate) fn join_pull(p: Pull) {
        let out = OUT_FRAMES.fetch_add(p.n as i64, Relaxed) + p.n as i64;
        let (callback, n, fill) = (p.callback as i64, p.n as i64, p.fill as i64);
        if p.trims > 0 {
            put([3, callback, 0, 0, n, fill]);
        }
        if p.zeroed > 0 && p.joined {
            put([5, callback, p.zeroed as i64, 0, n, fill]);
        }
        if p.callback % 1000 == 0 {
            put([4, callback, micros(Instant::now()) / 1000, IN_FRAMES.load(Relaxed), out, fill]);
        }
        let latency = p.latency.map_or(-1, |d| (d.as_secs_f64() * p.rate as f64).round() as i64);
        put_join(1, [micros(p.entry) - JOIN_FROM.load(Relaxed), n, fill, p.zeroed as i64, p.trims as i64, p.lost, latency]);
    }

    /// A join input callback of `n` frames, `age` after their capture.
    pub(crate) fn join_push(n: usize, age: Option<Duration>) {
        IN_FRAMES.fetch_add(n as i64, Relaxed);
        let age = age.map_or(-1, |d| d.as_micros() as i64);
        put_join(0, [micros(Instant::now()) - JOIN_FROM.load(Relaxed), n as i64, age, 0, 0, 0, 0]);
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
                    5 => format!("STARVE callback {cb}: n={c} fill={d} zeroed={a}"),
                    _ => format!("join  callback {cb}: {a} ms, pushed {b}, pulled {c}, fill {d}"),
                }
            })
            .collect()
    }

    /// The latest WASAPI run's pushes and pulls so far, in the order they were written, one line each:
    /// the time since the run was wired (ms), then a push's frames, its age at the callback (ms) and the
    /// frames pushed so far, or a pull's number, frames, the fill it found, the frames it zero-filled, its
    /// trims, the frames the device played dry before it, the playback delay reported and the frames
    /// asked for so far (a pull that primes takes none from the ring). The lines end before a record a
    /// callback is still writing.
    pub(crate) fn join_lines() -> Vec<String> {
        let run = JOIN_RUN.load(Relaxed);
        let (mut pushed, mut pulled, mut pulls) = (0, 0, 0);
        (0..JOIN_NEXT.load(Relaxed).min(JOIN_LEN))
            .map_while(|i| {
                let [tag, fields @ ..] = &JOIN[i];
                let tag = tag.load(Acquire);
                if tag / 2 != run {
                    return None;
                }
                let [t, n, a, b, c, d, e] = std::array::from_fn(|k| fields[k].load(Relaxed));
                let ms = t as f64 / 1e3;
                Some(if tag % 2 == 0 {
                    pushed += n;
                    format!("{ms:9.3} push n={n} age={:.2} pushed={pushed}", a as f64 / 1e3)
                } else {
                    (pulled, pulls) = (pulled + n, pulls + 1);
                    let mark = if c > 0 { " TRIM" } else if b > 0 { " zero" } else { "" };
                    format!("{ms:9.3} pull {pulls} n={n} fill={a} zeroed={b} trims={c} lost={d} latency={e} pulled={pulled}{mark}")
                })
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
/// gets their mean. Whether a value it converted was past full scale: the monitor joins the master
/// after the limiter, so nothing upstream bounds the sum.
fn write<T: SizedSample + FromSample<f32>>(data: &mut [T], channels: usize, left: &[f32], right: &[f32]) -> bool {
    let mut peak = 0.0f32;
    for ((frame, &l), &r) in data.chunks_exact_mut(channels).zip(left).zip(right) {
        match frame {
            [mono] => {
                let m = 0.5 * (l + r);
                peak = peak.max(m.abs());
                *mono = T::from_sample(m);
            }
            [a, b, rest @ ..] => {
                peak = peak.max(l.abs()).max(r.abs());
                *a = T::from_sample(l);
                *b = T::from_sample(r);
                rest.fill(T::EQUILIBRIUM);
            }
            [] => {}
        }
    }
    peak > 1.0
}

fn silence<T: Sample>(data: &mut [T]) {
    data.fill(T::EQUILIBRIUM);
}

#[cfg(test)]
mod tests;
