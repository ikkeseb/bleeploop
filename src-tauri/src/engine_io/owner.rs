//! OWNS: the device owner thread: every device transition (open, switch, close, a slot's channel change,
//! Share output, an ASIO driver switch, a loss and its fallback, shutdown) runs here, one at a time, over
//! a request channel with one-shot replies. The decisions are `transition.rs`'s; this carries them out.
//!
//! The owner takes the engine lock only while no stream runs, and builds engines here, never on a
//! callback (`Engine::new` allocates every buffer: ~130 MB and ~100 ms at 60-second lanes). Between
//! requests it polls every `POLL` for what the callbacks latched: a dead stream, a dead Share mirror;
//! and once a `GLITCH_EVERY` it logs the fault counters and glitch diagnostics that moved, or a callback
//! that ran past its period ([`GlitchWatch`]).

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering::{AcqRel, Acquire, Relaxed, Release}};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lf_engine::{Engine, EngineConfig, EngineHandle, SlotPort, SlotProcessor, TimedCommand, SLOT_COUNT};
use rtrb::{Consumer, Producer, RingBuffer};

use super::callback::{Run, Tap, TapEnd, LATENCY_SAMPLES, MAX_DEVICE_BLOCK};
use super::driver::{Driver, Mirror, Spec, StartError, Streams, Wiring};
use super::pipes::{self, PipeConfig};
use super::slot_host::ORPHAN;
use super::{transition, BlockLoad, Core, DeviceEvent, DeviceRequest, DeviceStatus, Ends, HostConfig, IoDiag, OpenError, Rt};
use crate::asio_startup::AsioStatusReport;
use crate::audio_output::AudioBackend;

/// How often the owner looks at the latches between requests.
const POLL: Duration = Duration::from_millis(20);
/// The longest the owner waits for a fade-out (the ramp plus two silent callbacks: 279 ms at 4096 frames
/// and 44.1 kHz) or for the callback to take a Share tap: a callback that does not answer by then is not
/// running.
const CALLBACK_WAIT: Duration = Duration::from_secs(1);
/// The longest a start waits for the first callbacks and their latency reports.
const START_WAIT: Duration = Duration::from_secs(2);
/// Device events kept for `EngineHost::take_device_events`; older ones drop.
const MAX_EVENTS: usize = 64;
/// The shortest time between two engine replacements after a fault: a unit that panics on every block
/// would otherwise rebuild the engine in a loop. A fault inside it waits for it to pass.
const FAULT_HOLDOFF: Duration = Duration::from_secs(10);
/// How often the owner looks at the fault counters for the release log (`GlitchWatch`).
pub(crate) const GLITCH_EVERY: Duration = Duration::from_secs(1);
/// The owner thread's stack: engines are built and moved on it (`Engine` is ~20 KB inline; a Windows
/// thread gets 1 MB by default), with room to spare.
const STACK: usize = 4 << 20;
/// Put a new engine (built at `config`'s rate) in place of the running one, with no device running,
/// and hand back the old one for the caller to drop off the audio thread (`None` when there was none,
/// or when a unit panicked on its way out and the old engine was leaked: `evict`). The old engine's
/// units go to their plugin owners: pending installs land first, then every unit is evicted into
/// `Core::evicted` (`SlotHost::take_evicted`). Every port is held across the swap, so a plugin owner
/// cannot slip an install into the old engine, and the new terms are published before any unit is
/// handed back, so its owner re-activates it at them. The device owner's rebuild, and the test
/// device's.
pub(crate) fn swap_engine(core: &Core, engine: Engine, handle: EngineHandle, config: EngineConfig) -> Option<Engine> {
    let EngineHandle { commands, events, slots, overview, session } = handle;
    let mut ports = core.ports.each_ref().map(|p| p.lock().unwrap_or_else(|e| e.into_inner()));
    core.rate.store(config.sample_rate, Relaxed);
    core.max_block.store(config.max_block as u32, Relaxed);
    let old = {
        let mut rt = rt(core);
        rt.faulted = false;
        core.fault.store(false, Relaxed);
        rt.engine.replace(engine)
    };
    core.rt.clear_poison();
    // Read before `evict`, which leaks the engine when a unit panics on its way out.
    let applied = old.as_ref().and_then(Engine::applied_mixes);
    let unsent: Vec<_> = old.as_ref().map(|e| e.unsent_toggles().collect()).unwrap_or_default();
    let old = old.and_then(|old| evict(core, &mut ports, old));
    for (port, new) in ports.iter_mut().zip(slots) {
        **port = Some(new);
    }
    {
        // The kept settings go in first, in order: they apply at the new engine's first block. Taken
        // before `ends`, as a sender takes them, so no batch splits around the replay. The old engine's
        // last mixes join them first, so the replay is the mix it applied: what its event ring still
        // holds, then each lane's mix read from the engine itself (off the audio thread now), which a
        // full ring may have refused (read before `evict`, so a leaked engine's counts too). One that
        // has not taken all the commands queued ahead of its first block reads as nothing (its replay
        // is not all applied). Its other events go with it, as its loops do.
        let mut settings = core.settings.lock().unwrap_or_else(|e| e.into_inner());
        let mut ends = core.ends.lock().unwrap_or_else(|e| e.into_inner());
        let gen = core.engine_gen.load(Acquire);
        if let Some(ends) = ends.as_mut() {
            super::drain(&mut settings, &mut ends.events, gen, |_| {});
        }
        if let Some((frame, mixes)) = applied {
            for (lane, mix) in mixes.iter().enumerate() {
                settings.mixed(gen, frame, lane as u8, mix);
            }
        }
        // The answers its full ring refused, owed at each setting's applied value: newer than any it
        // drained.
        for (toggle, on) in unsent {
            settings.toggled(gen, toggle, on);
        }
        let ends = ends.insert(Ends { commands, events, overview, session: Some(session) });
        let replay: Vec<_> = settings.replay().collect();
        settings.follow(core.engine_gen.fetch_add(1, Release) + 1);
        for command in replay {
            if ends.commands.push(TimedCommand { frame: None, command }).is_err() {
                core.counters.commands_full.fetch_add(1, Relaxed);
            } else {
                settings.pushed(&command);
            }
        }
    }
    old
}

/// Hand every unit of `old`, an engine nothing renders any more, to its plugin owner, stopped: into
/// `Core::evicted`, with whatever its ports (held by the caller) had returned. Returns the engine for
/// the caller to drop, or `None` when a unit panicked on its way out: the engine is leaked then, with
/// any unit still in it, rather than dropped here (a drop calls the plugin's DLL off its owner thread).
/// Such a slot stays held (`Core::holder`), so its owner's teardown leaks the plugin instead of
/// unloading it.
fn evict(core: &Core, ports: &mut [MutexGuard<'_, Option<SlotPort>>; SLOT_COUNT], mut old: Engine) -> Option<Engine> {
    let clean = catch_unwind(AssertUnwindSafe(|| {
        old.service_slots_idle();
        old.evict_slots();
    }))
    .is_ok();
    if !clean {
        core.counters.panics.fetch_add(1, Relaxed);
        log::error!("[engine_io] a unit panicked while it was evicted; leaking the old engine");
    }
    for (slot, port) in ports.iter_mut().enumerate() {
        if let Some(port) = port.as_mut() {
            while let Some(unit) = port.returned() {
                park(core, slot, unit);
            }
        }
    }
    if clean {
        Some(old)
    } else {
        std::mem::forget(old);
        None
    }
}

/// Keep an evicted unit for its plugin owner (the slot's holder), and free the slot. A unit its owner
/// abandoned (`SlotHost::abandon`) leaks, as does a second one, which the slot protocol rules out (one
/// unit per slot): never a drop here (a drop calls its DLL).
fn park(core: &Core, slot: usize, unit: Box<dyn SlotProcessor>) {
    let owner = core.holder[slot].swap(0, AcqRel);
    if owner == 0 || owner == ORPHAN {
        log::warn!("[engine_io] slot {slot}: an evicted unit has no owner left; leaking it");
        std::mem::forget(unit);
        return;
    }
    let mut waiting = core.evicted[slot].lock().unwrap_or_else(|e| e.into_inner());
    if waiting.is_some() {
        log::error!("[engine_io] slot {slot}: a second evicted unit came back while one waits; leaking it");
        std::mem::forget(unit);
    } else {
        *waiting = Some((owner, unit));
    }
}

/// WASAPI join: the ring's capacity and the fill it holds. The capacity holds the capture that runs
/// before the output opens (the Stage 1 spike measured ~300 ms; the prime drops it) without a drop. The
/// setpoint follows the pipe's rule (the largest push plus the largest pull plus ~3 ms:
/// `pipes::PipeConfig::setpoint`) for WASAPI shared mode's 10 ms periods on both sides; a device with
/// longer periods starves the join (`join_starves`).
const JOIN_CAPACITY_SECONDS: f64 = 0.5;
const JOIN_SETPOINT_SECONDS: f64 = 0.025;

type Reply<T> = SyncSender<Result<T, String>>;

/// A driver switch's own step: replace the cached ASIO driver (the coordinator's switch,
/// `audio_output::switch_asio_driver`; a test's fake). Run on the owner, with no ASIO run open.
pub(crate) type SwitchAsio = Box<dyn FnOnce() -> Result<AsioStatusReport, String> + Send>;

pub(crate) enum Request {
    /// A player's open, forced or not (`EngineHost::open`). The flag decides who owns the result: the
    /// first to claim it, the owner once the open finished or the caller on its timeout. An open the
    /// caller gave up on is undone (`Owner::undo_open`), and one it gave up on before the owner took it
    /// up never runs.
    Open(DeviceRequest, bool, Arc<AtomicBool>, SyncSender<Result<DeviceStatus, OpenError>>),
    Close(Reply<()>),
    SetSlotInputChannel(usize, Option<u32>, Reply<()>),
    SetShare(Option<String>, Reply<()>),
    /// `EngineHost::switch_asio`.
    SwitchAsio(SwitchAsio, Reply<AsioStatusReport>),
    Shutdown(SyncSender<()>),
}

/// The owner's thread and its request channel (in `Core`).
pub(crate) struct OwnerLink {
    pub(crate) tx: Sender<Request>,
    pub(crate) join: JoinHandle<()>,
}

/// Who opens: the player (`EngineHost::open`, and the reopen after a driver switch; `force` lets a switch
/// drop the loops at another rate), or the owner on its own (a loss's recovery or fallback, the device a
/// failed switch or a replaced engine reopens), which never refuses a rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum By {
    Player { force: bool },
    Owner,
}

/// The device that runs.
struct Active {
    request: DeviceRequest,
    spec: Spec,
    run: Arc<Run>,
    streams: Streams,
    block: u32,
}

/// The release log's view of the callbacks, which cannot log: once a `GLITCH_EVERY`, the fault counters
/// and glitch diagnostics that moved in that span (`IoDiag::moved_since`), with the output callbacks'
/// block times over the same span. A span in which a callback ran past its period
/// (`BlockLoad::over_budget`) gets a line too when no counter moved ("no counter moved", the count in
/// its block time). A crackle heard at a time then has a line to match. An xrun while every block stayed
/// well inside its period points away from the engine (the driver, USB, the system); blocks near or past
/// 100 % point at it.
pub(crate) struct GlitchWatch {
    at: Instant,
    diag: IoDiag,
    load: BlockLoad,
}

impl GlitchWatch {
    pub(crate) fn new(at: Instant, diag: IoDiag, load: BlockLoad) -> GlitchWatch {
        GlitchWatch { at, diag, load }
    }

    /// At `now`: once a span has passed, read the counters (`read`) and start the next span; the line to
    /// log when a fault counter moved in the one that ended or a callback in it ran past its period.
    pub(crate) fn tick(&mut self, now: Instant, read: impl FnOnce() -> (IoDiag, BlockLoad)) -> Option<String> {
        if now.saturating_duration_since(self.at) < GLITCH_EVERY {
            return None;
        }
        let (diag, load) = read();
        let span = load.since(&self.load);
        let moved = diag.moved_since(&self.diag).or_else(|| (span.over_budget() > 0).then(|| "no counter moved".to_string()));
        *self = GlitchWatch { at: now, diag, load };
        moved.map(|moved| format!("{moved} (block time {})", span.text()))
    }
}

/// The owner's end of the Share tap handoff (`callback::TapEnd` is the callback's).
struct TapHandoff {
    tx: Producer<Option<Box<dyn Tap>>>,
    back: Consumer<Box<dyn Tap>>,
}

struct Owner<D: Driver> {
    core: Arc<Core>,
    driver: D,
    config: HostConfig,
    active: Option<Active>,
    /// Share output's endpoint: remembered, and mirrored while ASIO plays.
    share: Option<String>,
    mirror: Option<Box<dyn Mirror>>,
    taps: TapHandoff,
    /// When a fault last replaced the engine (`FAULT_HOLDOFF`).
    replaced: Option<Instant>,
    glitches: GlitchWatch,
}

/// Start the owner thread for `core`.
pub(crate) fn spawn<D: Driver>(core: Arc<Core>, config: HostConfig, driver: D) -> std::io::Result<OwnerLink> {
    let (tx, rx) = mpsc::channel();
    let (tap_tx, tap_rx) = RingBuffer::new(1);
    let (back_tx, back_rx) = RingBuffer::new(2);
    rt(&core).taps = Some(TapEnd { rx: tap_rx, back: back_tx });
    let taps = TapHandoff { tx: tap_tx, back: back_rx };
    let join = std::thread::Builder::new()
        .name("lf-engine-owner".into())
        .stack_size(STACK)
        .spawn(move || {
            let glitches = GlitchWatch::new(Instant::now(), core.diag(), core.counters.block_load.snapshot());
            Owner { core, driver, config, active: None, share: None, mirror: None, taps, replaced: None, glitches }.serve(rx)
        })?;
    Ok(OwnerLink { tx, join })
}

/// The engine lock, for the owner (streams stopped) and setup. The callbacks never poison it (a panic
/// is caught inside the guard); should anything else, the state is still the owner's to clean up.
fn rt(core: &Core) -> std::sync::MutexGuard<'_, Rt> {
    core.rt.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl<D: Driver> Owner<D> {
    fn serve(mut self, rx: Receiver<Request>) {
        loop {
            match rx.recv_timeout(POLL) {
                Ok(Request::Shutdown(done)) => {
                    self.shutdown();
                    let _ = done.send(());
                    return;
                }
                Ok(request) => self.handle(request),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    self.shutdown();
                    return;
                }
            }
            self.poll();
        }
    }

    fn handle(&mut self, request: Request) {
        match request {
            Request::Open(request, force, claimed, reply) => {
                if claimed.load(Acquire) {
                    log::warn!("[engine_io] an open its caller gave up on while it waited: skipped");
                    let _ = reply.send(Err("the open was cancelled".to_string().into()));
                    return;
                }
                let previous = self.active.as_ref().map(|a| a.request.clone());
                let mut result = self.open(request, By::Player { force });
                let abandoned = claimed.compare_exchange(false, true, AcqRel, Acquire).is_err();
                if abandoned && result.is_ok() {
                    log::warn!("[engine_io] open finished after its caller timed out: undoing it");
                    self.undo_open(previous);
                    result = Err("the open was cancelled".to_string().into());
                }
                let _ = reply.send(result);
            }
            Request::Close(reply) => {
                self.stop(true);
                let _ = reply.send(Ok(()));
            }
            Request::SetSlotInputChannel(slot, channel, reply) => {
                let _ = reply.send(self.set_slot_input_channel(slot, channel));
            }
            Request::SetShare(endpoint, reply) => {
                let _ = reply.send(self.set_share(endpoint));
            }
            Request::SwitchAsio(switch, reply) => {
                let _ = reply.send(self.switch_asio(switch));
            }
            Request::Shutdown(_) => unreachable!("handled by serve"),
        }
    }

    /// What the callbacks latched: a faulted engine, a dead stream (a loss), a dead Share mirror.
    fn poll(&mut self) {
        if self.core.fault.swap(false, AcqRel) {
            self.replace_faulted();
        }
        if let Some(bits) = self.active.as_ref().map(|a| a.run.fault.load(Acquire)).filter(|&b| b != 0) {
            self.lose(bits);
        }
        if self.mirror.as_ref().is_some_and(|m| m.faulted()) {
            log::warn!("[engine_io] Share output's endpoint stopped: the mirror is off");
            self.close_mirror();
            self.share = None;
            self.event(DeviceEvent::ShareLost { reason: "the Share output endpoint stopped".to_string() });
        }
        let core = &self.core;
        if let Some(line) = self.glitches.tick(Instant::now(), || (core.diag(), core.counters.block_load.snapshot())) {
            match self.active.as_ref() {
                Some(a) => log::warn!("[engine_io] audio glitch: {line}, {:?} {} frames", a.request.backend, a.block),
                None => log::warn!("[engine_io] audio glitch: {line}, no device running"),
            }
        }
    }

    /// Open `request`, or switch to it. A request for the device that runs only changes its channels.
    /// A slot's pick the device lacks opens on auto (`transition::open_channels`). The player's open
    /// (`By::Player`) is refused, before anything stops, when it would build an engine at another rate
    /// while this one holds audio, unless forced; a switch of the player's that fails to start reopens the
    /// device it replaced. A size the ASIO driver refuses opens at its own (`start_at_own_size`).
    fn open(&mut self, request: DeviceRequest, by: By) -> Result<DeviceStatus, OpenError> {
        let driver = self.driver.asio_driver();
        if let Some(active) = self.active.as_ref().filter(|a| !a.run.faulted()) {
            if transition::same_device(&active.request, &request, (&active.spec.output_name, driver.as_deref())) {
                self.set_input_channels(request.input_channels)?;
                return Ok(self.status().ok_or_else(|| "the device stopped".to_string())?);
            }
        }
        let (spec, device) = self.driver.resolve(&request)?;
        let previous = self.active.as_ref().map(|a| a.request.clone());
        let healthy = self.active.as_ref().is_some_and(|a| !a.run.faulted());
        let steps = transition::steps(self.core.rate(), self.active.is_some(), healthy, Some(spec.rate));
        if let (By::Player { force: false }, true, Some(from)) = (by, steps.evict, self.core.rate()) {
            if self.core.holds_audio() {
                log::info!("[engine_io] a switch to {} Hz refused: the engine at {from} Hz holds audio", spec.rate);
                return Err(OpenError::RateChange { device: spec.output_name.clone(), from, to: spec.rate });
            }
        }
        let channels = open_channels(&spec, request.input_channels);
        if steps.stop {
            self.stop(steps.fade_out);
        }
        if steps.build {
            self.build(spec.rate, steps.evict);
        }
        let started = match self.start(request.clone(), spec, device, channels) {
            Err(StartError::Buffer(refused)) => self.start_at_own_size(request, refused, channels),
            started => started.map_err(String::from),
        };
        match started {
            Ok(status) => Ok(status),
            Err(error) => {
                log::error!("[engine_io] the device did not start: {error}");
                if let Some(previous) = previous.filter(|_| by != By::Owner) {
                    match self.open(previous, By::Owner) {
                        Ok(_) => log::warn!("[engine_io] reopened the previous device"),
                        Err(e) => log::error!("[engine_io] the previous device did not reopen either: {e}"),
                    }
                }
                Err(error.into())
            }
        }
    }

    /// The ASIO driver refused the size `request` asked for (`refused`): a driver whose sizes come in
    /// steps the probe's range cannot show. Open once more at the driver's own size; the running request
    /// keeps the player's, so asking for it again changes nothing, and the status says what runs.
    fn start_at_own_size(&mut self, request: DeviceRequest, refused: String, channels: [u32; SLOT_COUNT]) -> Result<DeviceStatus, String> {
        log::warn!("[engine_io] the driver refused the buffer ({refused}): opening at its own size");
        let (spec, device) = self.driver.resolve(&DeviceRequest { buffer: None, ..request.clone() })?;
        if self.core.rate() != Some(spec.rate) {
            return Err(format!("the driver moved to {} Hz between two opens", spec.rate));
        }
        self.start(request, spec, device, channels).map_err(String::from)
    }

    /// A driver switch (`EngineHost::switch_asio`): an ASIO run stops first (nothing may hold the driver
    /// while it is replaced), `switch` replaces the cached driver, then the run's request opens again on
    /// whatever driver is cached now, as the player's open would: refused while the loops cannot play at
    /// the new driver's rate (the UI asks, then opens again). On the owner, no open, reopen or recovery
    /// of its own can take the cache while the switch replaces it.
    fn switch_asio(&mut self, switch: SwitchAsio) -> Result<AsioStatusReport, String> {
        let ran = self.active.as_ref().filter(|a| a.spec.backend.is_asio()).map(|a| a.request.clone());
        if ran.is_some() {
            self.stop(true);
        }
        let report = switch();
        if let Some(request) = ran {
            match self.open(request, By::Player { force: false }) {
                Ok(status) => log::info!("[engine_io] reopened on \"{}\" after the driver switch", status.output_name),
                Err(error) => log::warn!("[engine_io] the device did not reopen after the driver switch: {error}"),
            }
        }
        report
    }

    /// An open its caller gave up on (it reported the open as failed): what ran before runs again, its
    /// channel included, or the device closes when none ran.
    fn undo_open(&mut self, previous: Option<DeviceRequest>) {
        let Some(previous) = previous else {
            self.stop(true);
            return;
        };
        if let Err(error) = self.open(previous, By::Owner) {
            log::error!("[engine_io] the device that ran before did not reopen: {error}");
        }
    }

    /// Build an engine at `rate`. The one it replaces (`evict`: another rate, or a fault) hands its
    /// units to their plugin owners (`SlotHost::take_evicted`, re-activated at the new rate) and is
    /// dropped here.
    fn build(&mut self, rate: u32, evict: bool) {
        let began = Instant::now();
        let config = EngineConfig { max_loop_seconds: self.config.max_loop_seconds, ..EngineConfig::new(rate) };
        let (engine, handle) = Engine::new(config);
        let old = swap_engine(&self.core, engine, handle, config);
        debug_assert!(old.is_none() || evict, "an engine is replaced only for another rate or a fault");
        drop(old);
        log::info!("[engine_io] engine built at {rate} Hz in {} ms{}", began.elapsed().as_millis(), if evict { "; the old one's units wait for their owners" } else { "" });
    }

    /// Start `spec`'s streams on the engine, wait for its first callbacks, then report it.
    fn start(&mut self, request: DeviceRequest, spec: Spec, device: D::Device, channels: [u32; SLOT_COUNT]) -> Result<DeviceStatus, StartError> {
        let run = Arc::new(Run::new(channels));
        {
            let mut rt = rt(&self.core);
            rt.handoff_len = 0;
            rt.in_cycles = 0;
            rt.out_cycles = 0;
        }
        self.core.align_frames.store(0, Relaxed);
        self.core.input_frames.store(0, Relaxed);
        let join = match spec.backend {
            AudioBackend::Asio => None,
            AudioBackend::Wasapi => Some(pipes::pipe(PipeConfig {
                in_rate: spec.in_rate,
                out_rate: spec.rate,
                // One stream per slot, interleaved.
                channels: SLOT_COUNT,
                capacity: (spec.in_rate as f64 * JOIN_CAPACITY_SECONDS).ceil() as usize,
                setpoint: JOIN_SETPOINT_SECONDS,
                max_pull: MAX_DEVICE_BLOCK,
            })?),
        };
        let wiring = Wiring::new(self.core.clone(), run.clone(), join);
        // Up before the first callback, and raised under the engine lock: a slot host servicing its
        // port finishes first, and one waiting for the lock finds it up and waits on its port instead
        // of taking the lock from under the callback (a lock miss).
        {
            let _rt = rt(&self.core);
            self.core.running.store(true, Release);
        }
        let started = match self.driver.start(device, &spec, wiring) {
            Ok(started) => started,
            Err(error) => {
                self.halted();
                return Err(error);
            }
        };
        let deadline = Instant::now() + START_WAIT;
        while run.callbacks.load(Relaxed) <= LATENCY_SAMPLES as u64 && !run.faulted() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        let failure = if run.faulted() {
            Some(format!("the device stopped as it started: {}", error_text(&run)))
        } else if run.callbacks.load(Relaxed) == 0 {
            Some(format!("the device opened but never called back within {} s", START_WAIT.as_secs()))
        } else {
            None
        };
        if let Some(failure) = failure {
            drop(started.streams);
            self.halted();
            // Its first callbacks may have started a take: it punches out as at any stop.
            self.punch_out();
            return Err(failure.into());
        }
        // ASIO: the size the callbacks deliver, whatever was asked (the driver's own, after a refusal).
        let block = match run.block.load(Relaxed) {
            delivered if spec.backend.is_asio() && delivered > 0 => delivered,
            _ if started.block > 0 => started.block,
            _ => spec.block,
        };
        let status = DeviceStatus {
            backend: spec.backend,
            sample_rate: spec.rate,
            block,
            input_name: if started.input_open { spec.input_name.clone() } else { String::new() },
            output_name: spec.output_name.clone(),
            input_open: started.input_open,
            input_channels: channels,
            // `status()` reads the alignment the callback renders with.
            align_frames: 0,
            input_frames: 0,
        };
        *self.core.device.lock().unwrap_or_else(|e| e.into_inner()) = Some(status);
        self.active = Some(Active { request, spec, run, streams: started.streams, block });
        if let Some(s) = self.status() {
            log::info!(
                "[engine_io] {:?} running: {} Hz, {block}-frame blocks, in \"{}\" channels {:?} (per slot), out \"{}\", align {} frames (input {})",
                s.backend, s.sample_rate, s.input_name, channels.map(|c| c + 1), s.output_name, s.align_frames, s.input_frames
            );
        }
        if let Some(endpoint) = self.share.clone() {
            if let Err(error) = self.open_mirror(&endpoint) {
                log::warn!("[engine_io] Share output did not open on \"{endpoint}\": {error}");
                self.share = None;
                self.event(DeviceEvent::ShareLost { reason: error });
            }
        }
        Ok(self.status().ok_or_else(|| "the device stopped".to_string())?)
    }

    /// No stream runs any more: slot hosts service their ports themselves, presses land unstamped.
    fn halted(&self) {
        self.core.running.store(false, Release);
        self.core.clock.clear();
        *self.core.device.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Stop the device: fade out (while it still plays), drop the output then the input, and punch out
    /// a take in flight (STATUS E3). The engine, its loops and its slots stay; the loops pause in place.
    fn stop(&mut self, fade: bool) {
        let Some(active) = self.active.take() else { return };
        if fade && !active.run.faulted() {
            active.run.fade_out.store(true, Release);
            let deadline = Instant::now() + CALLBACK_WAIT;
            while !active.run.faded.load(Acquire) && !active.run.faulted() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            if !active.run.faded.load(Acquire) {
                log::warn!("[engine_io] the fade-out did not finish within {} ms: stopping anyway", CALLBACK_WAIT.as_millis());
            }
        }
        drop(active.streams);
        // After the drop: a callback still in flight would publish over the cleared clock.
        self.halted();
        self.punch_out();
        let tap = {
            let mut rt = rt(&self.core);
            // A tap still on its way in comes out with the one in use: the mirror they fed is gone.
            let pending = rt.taps.as_mut().and_then(|end| end.rx.pop().ok()).flatten();
            (rt.tap.take(), pending)
        };
        drop(tap);
        while let Ok(old) = self.taps.back.pop() {
            drop(old);
        }
        // The tap is out: the mirror it fed goes last.
        self.mirror = None;
    }

    /// No stream runs: a take or overdub in flight ends after the last rendered frame and is kept.
    fn punch_out(&self) {
        let mut rt = rt(&self.core);
        let rt = &mut *rt;
        if rt.faulted {
            return;
        }
        let Some(engine) = rt.engine.as_mut() else { return };
        if catch_unwind(AssertUnwindSafe(|| engine.punch_out())).is_err() {
            self.core.counters.panics.fetch_add(1, Relaxed);
            self.core.latch_fault(rt);
        }
    }

    /// The engine faulted (a panic under its lock): it plays silence and no longer services its slots.
    /// Build a new one at the same rate (its loops are lost, its units go back to their owners) and
    /// restart the device that ran. Within `FAULT_HOLDOFF` of the last replacement the fault waits.
    fn replace_faulted(&mut self) {
        let Some(rate) = self.core.rate() else { return };
        if self.replaced.is_some_and(|at| at.elapsed() < FAULT_HOLDOFF) {
            self.core.fault.store(true, Release);
            return;
        }
        self.replaced = Some(Instant::now());
        log::error!("[engine_io] the engine faulted: replacing it at {rate} Hz; its loops are lost, its plugin units go back to their owners");
        let request = self.active.as_ref().map(|a| a.request.clone());
        self.stop(false);
        self.build(rate, true);
        self.event(DeviceEvent::EngineFaulted);
        if let Some(request) = request {
            if let Err(error) = self.open(request, By::Owner) {
                log::error!("[engine_io] the device did not restart after the engine was replaced: {error}");
            }
        }
    }

    /// A stream died: stop without a fade, report it, and try the fallbacks in order. One that runs at
    /// another rate builds a new engine and the loops leave with the old one, whether or not its streams
    /// then start: `LoopsDropped` says so, once.
    fn lose(&mut self, bits: u8) {
        let Some(active) = self.active.as_ref() else { return };
        let (lost, backend, reason) = (active.request.clone(), active.spec.backend, error_text(&active.run));
        let device = active.spec.output_name.clone();
        log::error!("[engine_io] the {backend:?} device was lost ({reason}); the engine and its slots stay");
        self.stop(false);
        self.event(DeviceEvent::Lost { backend, reason });
        // Read after the stop: its punch-out may have committed a take.
        let mut held = self.core.rate().filter(|_| self.core.holds_audio());
        let engine = self.core.engine_gen.load(Acquire);
        let (input, output) = (bits & super::callback::Side::Input as u8 != 0, bits & super::callback::Side::Output as u8 != 0);
        for next in transition::fallbacks(&lost, input, output) {
            let result = self.open(next.clone(), By::Owner);
            // A new engine since (here only another rate builds one), whether or not its streams then
            // started: the loops left with the old one.
            let dropped = match (held, self.core.rate()) {
                (Some(from), Some(to)) if self.core.engine_gen.load(Acquire) != engine => {
                    held = None;
                    Some((from, to))
                }
                _ => None,
            };
            let started = match result {
                Ok(status) => {
                    log::warn!("[engine_io] {} on {:?} \"{}\"", if next == lost { "recovered" } else { "fell back" }, status.backend, status.output_name);
                    self.event(if next == lost { DeviceEvent::Recovered(status) } else { DeviceEvent::Fallback(status) });
                    true
                }
                Err(error) => {
                    log::warn!("[engine_io] fallback {:?} did not open: {error}", next.backend);
                    false
                }
            };
            if let Some((from, to)) = dropped {
                log::warn!("[engine_io] the loops recorded at {from} Hz left with the old engine: \"{device}\" is gone and the engine was rebuilt at {to} Hz");
                self.event(DeviceEvent::LoopsDropped { device: device.clone(), from, to });
            }
            if started {
                return;
            }
        }
        log::error!("[engine_io] no fallback device opened; the engine waits for the next open");
    }

    /// Every slot's capture channel, in place, as an open takes them (a pick the device lacks: auto).
    fn set_input_channels(&mut self, channels: [Option<u32>; SLOT_COUNT]) -> Result<(), String> {
        let active = self.active.as_mut().ok_or("no audio device is open")?;
        let resolved = open_channels(&active.spec, channels);
        active.run.set_slot_channels(resolved);
        active.request.input_channels = channels;
        self.publish_channels(resolved);
        Ok(())
    }

    /// One slot's capture channel, in place; the other slot's stays, whatever it holds (an open may
    /// have taken auto for a pick the device lacks). Refused for a channel the device lacks: the UI
    /// offers only the device's inputs. An output-only device takes any pick: it captures nothing.
    fn set_slot_input_channel(&mut self, slot: usize, channel: Option<u32>) -> Result<(), String> {
        let active = self.active.as_mut().ok_or("no audio device is open")?;
        if slot >= SLOT_COUNT {
            return Err(format!("no slot {slot}"));
        }
        // The owner is the only writer: the other slot's pick is the one it last stored.
        let mut picks = active.run.slot_channels();
        picks[slot] = match transition::input_channel(active.spec.in_channels, channel) {
            Err(_) if active.spec.in_channels == 0 => 0,
            pick => pick?,
        };
        active.run.set_slot_channels(picks);
        active.request.input_channels[slot] = channel;
        self.publish_channels(picks);
        Ok(())
    }

    /// The channels each slot reads now, into the status.
    fn publish_channels(&self, channels: [u32; SLOT_COUNT]) {
        if let Some(status) = self.core.device.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            status.input_channels = channels;
        }
    }

    /// Share output: remember the pick; mirror it while ASIO plays (on WASAPI app capture takes the
    /// main output, so no mirror opens).
    fn set_share(&mut self, endpoint: Option<String>) -> Result<(), String> {
        self.share = endpoint.clone();
        if !self.active.as_ref().is_some_and(|a| a.spec.backend.is_asio()) {
            return Ok(());
        }
        self.close_mirror();
        if let Some(endpoint) = endpoint {
            if let Err(error) = self.open_mirror(&endpoint) {
                self.share = None;
                return Err(error);
            }
        }
        Ok(())
    }

    fn open_mirror(&mut self, endpoint: &str) -> Result<(), String> {
        let Some(active) = self.active.as_ref().filter(|a| a.spec.backend.is_asio()) else { return Ok(()) };
        let (rate, block) = (active.spec.rate, active.block);
        let (mirror, tap) = self.driver.open_share(endpoint, rate, block, &self.core)?;
        self.hand_tap(Some(tap))?;
        self.mirror = Some(mirror);
        log::info!("[engine_io] Share output mirrors the master to \"{endpoint}\"");
        Ok(())
    }

    /// Take the tap out of the callback, then drop the mirror it fed.
    fn close_mirror(&mut self) {
        if self.mirror.is_some() {
            if let Err(error) = self.hand_tap(None) {
                log::warn!("[engine_io] {error}");
            }
            self.mirror = None;
        }
    }

    /// Put `tap` (or none) into the callback. While streams run the callback swaps it in at a block
    /// start and hands the old one back (one is in the callback exactly while a mirror is open), which
    /// drops here, off the audio thread.
    fn hand_tap(&mut self, tap: Option<Box<dyn Tap>>) -> Result<(), String> {
        let Some(run) = self.active.as_ref().map(|a| a.run.clone()) else {
            let old = std::mem::replace(&mut rt(&self.core).tap, tap);
            drop(old);
            return Ok(());
        };
        let mut returned = self.mirror.is_none();
        if let Err(rtrb::PushError::Full(refused)) = self.taps.tx.push(tap) {
            drop(refused);
            return Err("the callback has not taken the last Share tap".to_string());
        }
        let deadline = Instant::now() + CALLBACK_WAIT;
        while (self.taps.tx.slots() == 0 || !returned) && !run.faulted() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
            while let Ok(old) = self.taps.back.pop() {
                drop(old);
                returned = true;
            }
        }
        Ok(())
    }

    fn event(&self, event: DeviceEvent) {
        let mut events = self.core.device_events.lock().unwrap_or_else(|e| e.into_inner());
        if events.len() == MAX_EVENTS {
            events.remove(0);
        }
        events.push(event);
    }

    fn status(&self) -> Option<DeviceStatus> {
        super::status(&self.core)
    }

    /// Close the device and drop the engine here. A unit still in it goes back to its plugin owner
    /// first, stopped, as at a rebuild.
    fn shutdown(&mut self) {
        self.stop(true);
        let engine = rt(&self.core).engine.take();
        let mut ports = self.core.ports.each_ref().map(|p| p.lock().unwrap_or_else(|e| e.into_inner()));
        let engine = engine.and_then(|engine| evict(&self.core, &mut ports, engine));
        for port in ports.iter_mut() {
            **port = None;
        }
        drop(ports);
        {
            let mut ends = self.core.ends.lock().unwrap_or_else(|e| e.into_inner());
            *ends = None;
            self.core.engine_gen.fetch_add(1, Release);
        }
        self.core.rate.store(0, Relaxed);
        self.core.max_block.store(0, Relaxed);
        drop(engine);
        log::info!("[engine_io] the device owner stopped");
    }
}

/// Each slot's channel at an open of `spec` ([`transition::open_channels`]): a slot whose pick the
/// device lacks reads auto, said once in the log.
fn open_channels(spec: &Spec, picks: [Option<u32>; SLOT_COUNT]) -> [u32; SLOT_COUNT] {
    let (channels, fell_back) = transition::open_channels(spec.in_channels, picks);
    for slot in (0..SLOT_COUNT).filter(|&slot| fell_back[slot]) {
        log::warn!(
            "[engine_io] slot {slot}: input {} is not on \"{}\" ({} inputs): it reads input {}",
            picks[slot].map_or(0, |c| c as u64 + 1),
            spec.input_name,
            spec.in_channels,
            channels[slot] + 1
        );
    }
    channels
}

/// The first fatal error a run latched, as text.
fn error_text(run: &Run) -> String {
    match run.error.lock() {
        Ok(error) => error.as_ref().map_or_else(|| "the stream stopped".to_string(), |e| e.to_string()),
        Err(_) => "the stream stopped".to_string(),
    }
}
