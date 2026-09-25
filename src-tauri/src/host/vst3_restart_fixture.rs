//! A VST3 component implemented in Rust through the real `IComponent` + `IAudioProcessor` vtables
//! (the `vst3` crate's `ComWrapper`), with counting lifecycle methods, activated by the production
//! `activate_component` and driven by the production RT producer (`vst3_producer_loop`) against a
//! plain heap ring in place of the WebView2 SharedBuffer. Proves the plugin-initiated restart
//! cycle: `setProcessing(0)` on the RT thread → `setActive(0)` → `setupProcessing` → `setActive(1)`
//! on the owner → RT resumed, with the layout re-queried and nothing dropped. The engine-mode tests
//! at the end create the same component through an in-process factory and load it with
//! `engine_slot` into a test device's engine. No DLL, audio device or GUI required.
use super::super::super::transport::HOP1_HEADER_BYTES;
use super::*;
use rtrb::RingBuffer;
use std::sync::atomic::{AtomicU64, AtomicUsize};
use std::thread::ThreadId;
use vst3::Steinberg::Vst::{IoMode, MediaType, RoutingInfo, SpeakerArrangement};
use vst3::Steinberg::{IBStream, TBool};

/// The thread every engine-mode test renders on (`engine_io::test_rig`).
const TEST_DEVICE_THREAD: &str = "lf-test-device";

/// Every lifecycle call the plugin sees, with the thread it saw it on. Owner-side calls
/// (`setActive`, `setupProcessing`) may lock; RT-side calls (`setProcessing`, `process`) record
/// through atomics only, so the fixture allocates nothing under the production RT alloc guard.
struct FixtureComponent {
    owner: ThreadId,
    active: AtomicBool,
    processing: AtomicBool,
    activations: AtomicUsize,
    deactivations: AtomicUsize,
    setups: AtomicUsize,
    starts: AtomicUsize,
    stops: AtomicUsize,
    processes: AtomicUsize,
    /// setActive/setupProcessing off the owner thread, setProcessing/process ON the owner thread,
    /// process while inactive or not processing, setupProcessing while active, setActive(0) while
    /// still processing — any is a violation of the VST3 call sequence.
    contract_violation: AtomicBool,
    /// What the plugin reports for its output bus / latency. The test changes them between
    /// activations to stand in for a `kIoChanged` / `kLatencyChanged` the host must re-query.
    out_channels: AtomicI32,
    latency: AtomicU32,
    /// A plugin that reports a restart from inside `setActive(1)` (a `kLatencyChanged` once its
    /// buffers exist is common). The cycle must consume it, not schedule another cycle.
    raise_on_activate: Option<Arc<RestartFlags>>,
    reject_processing_start: bool,
    main_thread_calls: Mutex<Vec<ThreadId>>,
    /// A separated edit controller's class (the engine-mode factory's); `None` = no controller.
    controller_cid: Option<TUID>,
    /// The audio input bus's channel count; 0 = no input bus, an instrument. With an input the
    /// component is an effect that echoes each input channel to its output.
    inputs: AtomicI32,
    /// The last `setupProcessing`'s sample rate (f64 bits) and max block.
    setup_rate: AtomicU64,
    setup_max_frames: AtomicI32,
    /// Written to every output channel on each process call (f32 bits; 0 leaves them silent).
    output_level: AtomicU32,
    /// setProcessing/process calls on a thread other than the test device's (engine-mode tests).
    rt_calls_off_device: AtomicUsize,
    note_ons: AtomicUsize,
    note_offs: AtomicUsize,
    last_pitch: AtomicI32,
    last_note_offset: AtomicI32,
    param_points: AtomicUsize,
    last_param: AtomicU32,
    last_param_value: AtomicU64,
    /// Lifecycle order: each of these takes the next `seq` value when its call happens.
    seq: AtomicUsize,
    stop_seq: AtomicUsize,
    deactivate_seq: AtomicUsize,
    terminate_seq: AtomicUsize,
}

impl FixtureComponent {
    fn new(raise_on_activate: Option<Arc<RestartFlags>>, reject_processing_start: bool) -> Self {
        Self {
            owner: std::thread::current().id(),
            active: AtomicBool::new(false),
            processing: AtomicBool::new(false),
            activations: AtomicUsize::new(0),
            deactivations: AtomicUsize::new(0),
            setups: AtomicUsize::new(0),
            starts: AtomicUsize::new(0),
            stops: AtomicUsize::new(0),
            processes: AtomicUsize::new(0),
            contract_violation: AtomicBool::new(false),
            out_channels: AtomicI32::new(2),
            latency: AtomicU32::new(0),
            raise_on_activate,
            reject_processing_start,
            main_thread_calls: Mutex::new(Vec::new()),
            controller_cid: None,
            inputs: AtomicI32::new(0),
            setup_rate: AtomicU64::new(0),
            setup_max_frames: AtomicI32::new(0),
            output_level: AtomicU32::new(0),
            rt_calls_off_device: AtomicUsize::new(0),
            note_ons: AtomicUsize::new(0),
            note_offs: AtomicUsize::new(0),
            last_pitch: AtomicI32::new(-1),
            last_note_offset: AtomicI32::new(-1),
            param_points: AtomicUsize::new(0),
            last_param: AtomicU32::new(0),
            last_param_value: AtomicU64::new(0),
            seq: AtomicUsize::new(0),
            stop_seq: AtomicUsize::new(0),
            deactivate_seq: AtomicUsize::new(0),
            terminate_seq: AtomicUsize::new(0),
        }
    }
    fn tick(&self, at: &AtomicUsize) {
        at.store(self.seq.fetch_add(1, Relaxed) + 1, Relaxed);
    }
    fn off_device(&self) {
        if std::thread::current().name() != Some(TEST_DEVICE_THREAD) {
            self.rt_calls_off_device.fetch_add(1, Relaxed);
        }
    }
    fn on_owner(&self) -> bool {
        self.owner == std::thread::current().id()
    }
    fn violate_if(&self, cond: bool) {
        if cond {
            self.contract_violation.store(true, Relaxed);
        }
    }
}

impl Class for FixtureComponent {
    type Interfaces = (IComponent, IAudioProcessor);
}

impl IPluginBaseTrait for FixtureComponent {
    unsafe fn initialize(&self, _context: *mut FUnknown) -> tresult {
        kResultOk
    }
    unsafe fn terminate(&self) -> tresult {
        self.tick(&self.terminate_seq);
        kResultOk
    }
}

impl IComponentTrait for FixtureComponent {
    unsafe fn getControllerClassId(&self, class_id: *mut TUID) -> tresult {
        match self.controller_cid {
            Some(cid) => {
                *class_id = cid;
                kResultOk
            }
            None => kResultFalse,
        }
    }
    unsafe fn setIoMode(&self, _mode: IoMode) -> tresult {
        kResultOk
    }
    unsafe fn getBusCount(&self, r#type: MediaType, dir: int32) -> int32 {
        let audio = r#type == MediaTypes_::kAudio as i32;
        let event = r#type == MediaTypes_::kEvent as i32;
        let input = dir == BusDirections_::kInput as i32;
        match (audio, event, input) {
            (true, _, false) => 1, // one audio output bus
            (true, _, true) => (self.inputs.load(Relaxed) > 0) as int32, // an effect's input bus
            (_, true, true) => 1,  // one event input bus
            _ => 0,
        }
    }
    unsafe fn getBusInfo(
        &self,
        r#type: MediaType,
        dir: int32,
        index: int32,
        bus: *mut BusInfo,
    ) -> tresult {
        let input = dir == BusDirections_::kInput as i32;
        if r#type != MediaTypes_::kAudio as i32 || index != 0 || (input && self.inputs.load(Relaxed) == 0) {
            return kInvalidArgument;
        }
        // SAFETY: the host passes a valid, zeroed BusInfo.
        let bi = &mut *bus;
        bi.mediaType = r#type;
        bi.direction = dir;
        bi.channelCount = if input { self.inputs.load(Relaxed) } else { self.out_channels.load(Relaxed) };
        kResultOk
    }
    unsafe fn getRoutingInfo(&self, _in: *mut RoutingInfo, _out: *mut RoutingInfo) -> tresult {
        kResultFalse
    }
    unsafe fn activateBus(&self, _t: MediaType, _d: int32, _i: int32, _s: TBool) -> tresult {
        self.violate_if(!self.on_owner());
        kResultOk
    }
    unsafe fn setActive(&self, state: TBool) -> tresult {
        self.violate_if(!self.on_owner());
        self.main_thread_calls.lock().unwrap().push(std::thread::current().id());
        if state != 0 {
            self.violate_if(self.active.swap(true, Relaxed));
            self.activations.fetch_add(1, Relaxed);
            if let Some(flags) = &self.raise_on_activate {
                flags.raise(RestartFlags_::kLatencyChanged);
            }
        } else {
            self.violate_if(!self.active.swap(false, Relaxed) || self.processing.load(Relaxed));
            self.tick(&self.deactivate_seq);
            self.deactivations.fetch_add(1, Relaxed);
        }
        kResultOk
    }
    unsafe fn setState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }
    unsafe fn getState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }
}

impl IAudioProcessorTrait for FixtureComponent {
    unsafe fn setBusArrangements(
        &self,
        _inputs: *mut SpeakerArrangement,
        _num_ins: int32,
        _outputs: *mut SpeakerArrangement,
        _num_outs: int32,
    ) -> tresult {
        self.violate_if(!self.on_owner() || self.active.load(Relaxed));
        kResultOk
    }
    unsafe fn getBusArrangement(
        &self,
        _dir: int32,
        _index: int32,
        arr: *mut SpeakerArrangement,
    ) -> tresult {
        *arr = vst3::Steinberg::Vst::SpeakerArr::kStereo;
        kResultOk
    }
    unsafe fn canProcessSampleSize(&self, symbolic_sample_size: int32) -> tresult {
        if symbolic_sample_size == SymbolicSampleSizes_::kSample32 as i32 {
            kResultOk
        } else {
            kResultFalse
        }
    }
    unsafe fn getLatencySamples(&self) -> uint32 {
        self.latency.load(Relaxed)
    }
    unsafe fn setupProcessing(&self, setup: *mut ProcessSetup) -> tresult {
        self.violate_if(!self.on_owner() || self.active.load(Relaxed));
        self.setup_rate.store((*setup).sampleRate.to_bits(), Relaxed);
        self.setup_max_frames.store((*setup).maxSamplesPerBlock, Relaxed);
        self.main_thread_calls.lock().unwrap().push(std::thread::current().id());
        self.setups.fetch_add(1, Relaxed);
        kResultOk
    }
    unsafe fn setProcessing(&self, state: TBool) -> tresult {
        self.violate_if(self.on_owner() || !self.active.load(Relaxed));
        self.off_device();
        if state != 0 {
            self.starts.fetch_add(1, Relaxed);
            if self.reject_processing_start {
                return kResultFalse;
            }
            self.violate_if(self.processing.swap(true, Relaxed));
        } else {
            self.violate_if(!self.processing.swap(false, Relaxed));
            self.tick(&self.stop_seq);
            self.stops.fetch_add(1, Relaxed);
        }
        kResultOk
    }
    unsafe fn process(&self, data: *mut ProcessData) -> tresult {
        self.violate_if(
            self.on_owner() || !self.active.load(Relaxed) || !self.processing.load(Relaxed),
        );
        self.off_device();
        // SAFETY: the host's ProcessData, its lists and its output rows are valid for this call;
        // the lists are read through their own vtables and at most `numSamples` frames are written.
        let data = &*data;
        if let Some(list) = ComRef::<IEventList>::from_raw(data.inputEvents) {
            for i in 0..list.getEventCount() {
                let mut e: Event = std::mem::zeroed();
                if list.getEvent(i, &mut e) != kResultOk {
                    continue;
                }
                if e.r#type == vst3::Steinberg::Vst::Event_::EventTypes_::kNoteOnEvent as u16 {
                    self.last_pitch.store(e.__field0.noteOn.pitch as i32, Relaxed);
                    self.last_note_offset.store(e.sampleOffset, Relaxed);
                    self.note_ons.fetch_add(1, Relaxed);
                } else if e.r#type == vst3::Steinberg::Vst::Event_::EventTypes_::kNoteOffEvent as u16 {
                    self.note_offs.fetch_add(1, Relaxed);
                }
            }
        }
        if let Some(changes) = ComRef::<IParameterChanges>::from_raw(data.inputParameterChanges) {
            for i in 0..changes.getParameterCount() {
                let Some(queue) = ComRef::<IParamValueQueue>::from_raw(changes.getParameterData(i)) else {
                    continue;
                };
                let (mut offset, mut value) = (0, 0.0);
                if queue.getPoint(0, &mut offset, &mut value) == kResultOk {
                    self.last_param.store(queue.getParameterId(), Relaxed);
                    self.last_param_value.store(value.to_bits(), Relaxed);
                    self.param_points.fetch_add(1, Relaxed);
                }
            }
        }
        let level = f32::from_bits(self.output_level.load(Relaxed));
        if data.numInputs > 0 && data.numOutputs > 0 {
            let (input, output) = (&*data.inputs, &*data.outputs);
            for c in 0..input.numChannels.min(output.numChannels) as usize {
                let from = std::slice::from_raw_parts(*input.__field0.channelBuffers32.add(c), data.numSamples as usize);
                let to = *output.__field0.channelBuffers32.add(c);
                std::slice::from_raw_parts_mut(to, data.numSamples as usize).copy_from_slice(from);
            }
        } else if level != 0.0 && data.numOutputs > 0 {
            let bus = &*data.outputs;
            for c in 0..bus.numChannels as usize {
                let row = *bus.__field0.channelBuffers32.add(c);
                std::slice::from_raw_parts_mut(row, data.numSamples as usize).fill(level);
            }
        }
        self.processes.fetch_add(1, Relaxed);
        kResultOk // silent unless a test sets a level: the host zeroes the rows before every call
    }
    unsafe fn getTailSamples(&self) -> uint32 {
        0
    }
}

/// Stands in for the JS consumer (the worklet) for the rest of a test: a thread keeps the hop-1 read
/// cursor (header word [1]) caught up with the write cursor ([0]), through the restart too. Like the
/// worklet's render thread it runs at audio priority (MMCSS Pro Audio, as the producer does), so the
/// test threads beside it cannot starve it, and it spins (`yield_now`): a 1 ms sleep can round up to
/// the 15.6 ms timer tick Windows grants, past the ring's slack. Stops and joins on drop, so it never
/// outlives the ring it reads.
struct Drain {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Drain {
    /// SAFETY: `header` points at the ring's two 4-byte-aligned cursor words, which outlive the
    /// `Drain` (drop it before the ring); the RT thread is the sole writer of [0] and the sole reader
    /// of [1], mirroring the WebView2 contract.
    unsafe fn start(header: *mut u32) -> Drain {
        let addr = header as usize;
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let join = std::thread::spawn(move || {
            crate::engine_io::promote_pro_audio();
            let header = addr as *mut u32;
            // SAFETY: the caller's contract above.
            let (write_idx, read_idx) = unsafe { (AtomicU32::from_ptr(header), AtomicU32::from_ptr(header.add(1))) };
            while !stopped.load(Acquire) {
                read_idx.store(write_idx.load(Acquire), Release);
                std::thread::yield_now();
            }
        });
        Drain { stop, join: Some(join) }
    }
}

impl Drop for Drain {
    fn drop(&mut self) {
        self.stop.store(true, Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Spin until `pred` holds or `ms` elapse; returns whether it held.
fn wait_for(ms: u64, pred: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < deadline {
        if pred() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    pred()
}

#[test]
fn plugin_requested_restart_cycles_activation_on_the_owner_without_reload() {
    const CAP_FRAMES: u32 = 1024; // power of two (the hop-1 ring masks)
    const MAX_FRAMES: u32 = 512;
    const RATE: f64 = 48_000.0;

    let restart = Arc::new(RestartFlags::default());
    let fixture = ComWrapper::new(FixtureComponent::new(Some(restart.clone()), false));
    let s: &FixtureComponent = &fixture;
    let component = fixture.to_com_ptr::<IComponent>().unwrap();
    // The production host reaches the processor the same way: a cast on the component.
    let processor = component.cast::<IAudioProcessor>().unwrap();

    // A plain heap ring stands in for the WebView2 SharedBuffer: header + f32 data, 4-byte aligned.
    let mut ring = vec![0u32; HOP1_HEADER_BYTES / 4 + CAP_FRAMES as usize];
    let cfg = Vst3RtConfig {
        slot: 0,
        shared_ptr: ring.as_mut_ptr() as usize,
        cap_frames: CAP_FRAMES,
        max_frames: MAX_FRAMES,
        sample_rate: RATE,
        device_rate: RATE,
    };
    let diag = Arc::new(ProducerDiag::new());
    diag.init(RATE, RATE, MAX_FRAMES, 2, CAP_FRAMES as usize, 256, 1);

    // Load: the one activation sequence.
    let mut activation =
        unsafe { activate_component(&component, &processor, RATE, MAX_FRAMES) }.unwrap();
    assert_eq!(
        activation,
        Activation {
            out_channels: 2,
            in_channels: 0,
            latency_frames: 0
        }
    );
    assert_eq!(s.setups.load(Relaxed), 1);
    assert_eq!(s.activations.load(Relaxed), 1);
    // A plugin reporting from inside setActive(1) at LOAD is drained by the owner loop's first turn
    // like any other request; here the test stands in for that turn.
    assert_eq!(restart.take(), RestartFlags_::kLatencyChanged);

    let (_event_tx, event_rx) = RingBuffer::<PluginEvent>::new(16);
    let (_in_tx, in_rx) = RingBuffer::<f32>::new(16);
    let (mon_tx, _mon_rx) = RingBuffer::<f32>::new(16);
    let mut rt_guard = Some(
        spawn_vst3_rt(
            &cfg,
            processor,
            RtRings {
                event_rx,
                in_rx,
                mon_tx,
            },
            activation,
            128,
            super::super::BLOCK_CONFIG_GEN.load(Acquire),
            0.0,
            diag.clone(),
        )
        .unwrap(),
    );
    assert!(
        wait_for(2000, || s.processes.load(Relaxed) > 4),
        "the production RT loop must process the fixture"
    );
    assert_eq!(s.starts.load(Relaxed), 1, "setProcessing(1) ran once, on the RT thread");

    // The JS consumer, kept caught up from here to the end: the producer runs well past one ring
    // capacity first, so a restart that forgot the cursor would wrap `used` and drop every block
    // (the 2026-09-10 CLAP runtime finding).
    // SAFETY: the ring outlives `drain` (dropped before it at the end); see `Drain::start`.
    let drain = unsafe { Drain::start(ring.as_mut_ptr()) };
    // SAFETY: header word [0], 4-byte aligned; this thread only reads it.
    let write_idx = unsafe { AtomicU32::from_ptr(ring.as_mut_ptr()) };
    assert!(
        wait_for(4000, || write_idx.load(Acquire) > CAP_FRAMES * 2),
        "the producer must publish past one ring capacity with a draining reader"
    );
    let written_before_restart = write_idx.load(Acquire);
    let dropped_before_restart = diag.frames_dropped.load(Relaxed);

    // The request arrives from a foreign thread (a plugin ignoring the main-thread rule) and must
    // allocate nothing under the RT alloc guard: the handler body is `RestartFlags::raise`.
    let raiser = restart.clone();
    std::thread::spawn(move || {
        #[cfg(debug_assertions)]
        let allocations_before = super::super::super::rt_alloc::RT_ALLOCS.load(Relaxed);
        {
            #[cfg(debug_assertions)]
            let _guard = super::super::super::rt_alloc::guard();
            raiser.raise(RestartFlags_::kIoChanged);
            raiser.raise(RestartFlags_::kLatencyChanged);
        }
        #[cfg(debug_assertions)]
        assert_eq!(
            super::super::super::rt_alloc::RT_ALLOCS.load(Relaxed),
            allocations_before,
            "raising a restart must allocate nothing"
        );
    })
    .join()
    .unwrap();
    assert_eq!(
        s.deactivations.load(Relaxed),
        0,
        "the request itself must not run lifecycle code inline"
    );

    // Owner turn: the burst drains once, OR-ed, and the cycle runs. Meanwhile the plugin changed
    // what it reports (mono out, 64 frames latency) — the cycle must re-query, not reuse.
    let flags = restart.take();
    assert_eq!(flags, RestartFlags_::kIoChanged | RestartFlags_::kLatencyChanged);
    assert_eq!(restart.take(), 0, "a burst coalesces into one cycle");
    s.out_channels.store(1, Relaxed);
    s.latency.store(64, Relaxed);
    let processed_before = s.processes.load(Relaxed);
    service_vst3_restart(
        flags,
        &component,
        &mut rt_guard,
        &cfg,
        &mut activation,
        &restart,
        &diag,
    )
    .unwrap();
    assert!(rt_guard.is_some(), "a fresh RT producer replaces the joined one");
    assert_eq!(s.stops.load(Relaxed), 1, "setProcessing(0) ran on the RT thread before setActive(0)");
    assert_eq!(s.deactivations.load(Relaxed), 1);
    assert_eq!(s.setups.load(Relaxed), 2, "setupProcessing ran again while inactive");
    assert_eq!(s.activations.load(Relaxed), 2, "setActive(1) ran again on the same component");
    assert_eq!(
        activation,
        Activation {
            out_channels: 1,
            in_channels: 0,
            latency_frames: 64
        },
        "the cycle re-queried the layout the plugin now reports"
    );
    assert_eq!(
        restart.take(),
        0,
        "the kLatencyChanged the plugin raised from inside setActive(1) was consumed by the cycle, not queued as another"
    );
    assert!(
        wait_for(2000, || s.starts.load(Relaxed) == 2
            && s.processes.load(Relaxed) > processed_before + 4),
        "the respawned RT producer must resume processing"
    );
    // The hop-1 write cursor continues from where the joined producer left it (the JS reader is
    // still at its old position), and — with the reader kept caught up — nothing is dropped.
    assert!(
        wait_for(2000, || write_idx.load(Acquire) > written_before_restart + CAP_FRAMES),
        "the respawned producer must continue the published write cursor, not restart it at 0"
    );
    assert_eq!(
        diag.frames_dropped.load(Relaxed),
        dropped_before_restart,
        "no hop-1 frame may be dropped across the restart while the reader keeps up"
    );

    // Unload path: the same guard handshake, then deactivate on the owner (as `teardown` does).
    let exit = rt_guard.take().unwrap().stop_and_join().unwrap();
    assert_eq!(exit.period_frames, 128, "the respawn continued at the reconciled block");
    unsafe {
        assert_eq!(component.setActive(0), kResultOk);
    }
    drop(exit);
    assert_eq!(s.stops.load(Relaxed), 2);
    assert_eq!(s.deactivations.load(Relaxed), 2);
    assert!(
        !s.contract_violation.load(Relaxed),
        "setActive/setupProcessing on the owner thread only, setProcessing/process on the RT thread only, never process while inactive, never setActive(0) while processing"
    );
    let owner = std::thread::current().id();
    assert!(
        s.main_thread_calls.lock().unwrap().iter().all(|t| *t == owner),
        "every setActive/setupProcessing happened on the owner thread"
    );
    assert_eq!(diag.rt_faults.load(Relaxed), 0, "no RT fault was latched across the cycle");
    drop(component);
    drop(drain);
    drop(ring);
}

#[test]
fn a_failed_reactivation_leaves_the_slot_silent_but_serviceable() {
    const CAP_FRAMES: u32 = 1024;
    const MAX_FRAMES: u32 = 512;
    const RATE: f64 = 48_000.0;

    let restart = Arc::new(RestartFlags::default());
    let fixture = ComWrapper::new(FixtureComponent::new(None, false));
    let s: &FixtureComponent = &fixture;
    let component = fixture.to_com_ptr::<IComponent>().unwrap();
    let processor = component.cast::<IAudioProcessor>().unwrap();
    let mut ring = vec![0u32; HOP1_HEADER_BYTES / 4 + CAP_FRAMES as usize];
    let cfg = Vst3RtConfig {
        slot: 1,
        shared_ptr: ring.as_mut_ptr() as usize,
        cap_frames: CAP_FRAMES,
        max_frames: MAX_FRAMES,
        sample_rate: RATE,
        device_rate: RATE,
    };
    let diag = Arc::new(ProducerDiag::new());
    diag.init(RATE, RATE, MAX_FRAMES, 2, CAP_FRAMES as usize, 256, 1);
    let mut activation =
        unsafe { activate_component(&component, &processor, RATE, MAX_FRAMES) }.unwrap();
    let (_event_tx, event_rx) = RingBuffer::<PluginEvent>::new(16);
    let (_in_tx, in_rx) = RingBuffer::<f32>::new(16);
    let (mon_tx, _mon_rx) = RingBuffer::<f32>::new(16);
    let mut rt_guard = Some(
        spawn_vst3_rt(
            &cfg,
            processor,
            RtRings {
                event_rx,
                in_rx,
                mon_tx,
            },
            activation,
            128,
            super::super::BLOCK_CONFIG_GEN.load(Acquire),
            0.0,
            diag.clone(),
        )
        .unwrap(),
    );
    assert!(wait_for(2000, || s.processes.load(Relaxed) > 4));

    // The plugin now reports an output channel count the host refuses (0 channels), so the
    // re-activation fails after setActive(0): the slot goes silent, the component stays inactive.
    s.out_channels.store(0, Relaxed);
    let err = service_vst3_restart(
        RestartFlags_::kIoChanged,
        &component,
        &mut rt_guard,
        &cfg,
        &mut activation,
        &restart,
        &diag,
    )
    .unwrap_err();
    assert!(err.contains("VST3 output bus 0"), "{err}");
    assert!(rt_guard.is_none(), "no producer runs after a failed re-activation");
    assert_eq!(s.stops.load(Relaxed), 1);
    assert_eq!(s.deactivations.load(Relaxed), 1);
    assert_eq!(s.activations.load(Relaxed), 1, "setActive(1) never ran on the refused layout");
    assert_eq!(activation.out_channels, 2, "the last good activation is kept for the log");
    // A second request has nothing to restart and says so; unload's setActive(0) is harmless.
    let err = service_vst3_restart(
        RestartFlags_::kIoChanged,
        &component,
        &mut rt_guard,
        &cfg,
        &mut activation,
        &restart,
        &diag,
    )
    .unwrap_err();
    assert!(err.contains("no RT producer"), "{err}");
    assert!(!s.contract_violation.load(Relaxed));
    drop(component);
    drop(ring);
}

#[test]
fn rejected_processing_start_never_enters_process_and_latches_the_fault() {
    const CAP_FRAMES: u32 = 1024;
    const MAX_FRAMES: u32 = 512;
    const RATE: f64 = 48_000.0;

    let fixture = ComWrapper::new(FixtureComponent::new(None, true));
    let s: &FixtureComponent = &fixture;
    let component = fixture.to_com_ptr::<IComponent>().unwrap();
    let processor = component.cast::<IAudioProcessor>().unwrap();
    let mut ring = vec![0u32; HOP1_HEADER_BYTES / 4 + CAP_FRAMES as usize];
    let cfg = Vst3RtConfig {
        slot: 0,
        shared_ptr: ring.as_mut_ptr() as usize,
        cap_frames: CAP_FRAMES,
        max_frames: MAX_FRAMES,
        sample_rate: RATE,
        device_rate: RATE,
    };
    let diag = Arc::new(ProducerDiag::new());
    diag.init(RATE, RATE, MAX_FRAMES, 2, CAP_FRAMES as usize, 256, 1);
    let activation =
        unsafe { activate_component(&component, &processor, RATE, MAX_FRAMES) }.unwrap();
    let (_event_tx, event_rx) = RingBuffer::<PluginEvent>::new(16);
    let (_in_tx, in_rx) = RingBuffer::<f32>::new(16);
    let (mon_tx, _mon_rx) = RingBuffer::<f32>::new(16);
    let guard = spawn_vst3_rt(
        &cfg,
        processor,
        RtRings {
            event_rx,
            in_rx,
            mon_tx,
        },
        activation,
        128,
        super::super::BLOCK_CONFIG_GEN.load(Acquire),
        0.0,
        diag.clone(),
    )
    .unwrap();

    // The RT thread returns before its loop, so the join alone is deterministic here.
    let exit = guard.stop_and_join().unwrap();
    assert_eq!(
        s.starts.load(Relaxed),
        1,
        "setProcessing(1) was attempted once"
    );
    assert_eq!(
        s.processes.load(Relaxed),
        0,
        "process must not run after setProcessing(1) failed"
    );
    assert_eq!(
        s.stops.load(Relaxed),
        0,
        "a processor that never started must not be stopped"
    );
    assert_ne!(
        diag.rt_faults.load(Relaxed) & RtFault::Vst3Process as u32,
        0,
        "the rejected start must latch the VST3 process fault"
    );
    unsafe {
        assert_eq!(component.setActive(0), kResultOk);
    }
    drop(exit);
    assert_eq!(s.deactivations.load(Relaxed), 1);
    assert!(!s.contract_violation.load(Relaxed));
    drop(component);
    drop(ring);
}

// ── engine mode: the same component as a unit inside a test device's engine (`engine_slot`) ─────

mod engine {
    use super::super::super::engine_slot::{self, EngineSlotEvent, EngineSlotHandle, PluginFormat};
    use super::super::controller_tests::FixtureController;
    use super::*;
    use crate::engine_io::test_rig::TestDevice;
    use lf_engine::{Command, NoteTarget, SlotKind, TimedCommand};
    use vst3::Steinberg::PFactoryInfo;

    const fn tuid(bytes: &[u8; 16]) -> TUID {
        let mut t = [0; 16];
        let mut i = 0;
        while i < 16 {
            t[i] = bytes[i] as _;
            i += 1;
        }
        t
    }
    const COMPONENT_CID: TUID = tuid(b"BleepLoopEngineC");
    const CONTROLLER_CID: TUID = tuid(b"BleepLoopEngineE");

    /// What the factory made on the owner thread, for the test to watch.
    #[derive(Default)]
    struct Made {
        component: Mutex<Option<ComWrapper<FixtureComponent>>>,
        controller: Mutex<Option<ComWrapper<FixtureController>>>,
    }

    impl Made {
        fn component(&self) -> ComWrapper<FixtureComponent> {
            self.component.lock().unwrap().clone().expect("the component was created")
        }
        fn controller(&self) -> ComWrapper<FixtureController> {
            self.controller.lock().unwrap().clone().expect("the controller was created")
        }
    }

    /// An in-process `IPluginFactory` with two classes: the fixture component (separated, as a
    /// JUCE plugin is) and its edit controller, a `FixtureController` listing two params.
    struct FixtureFactory {
        made: Arc<Made>,
        latency: u32,
        output_level: f32,
        inputs: i32,
    }

    impl Class for FixtureFactory {
        type Interfaces = (IPluginFactory,);
    }

    impl IPluginFactoryTrait for FixtureFactory {
        unsafe fn getFactoryInfo(&self, _info: *mut PFactoryInfo) -> tresult {
            kResultFalse
        }
        unsafe fn countClasses(&self) -> int32 {
            2
        }
        unsafe fn getClassInfo(&self, index: int32, info: *mut PClassInfo) -> tresult {
            let (cid, name) = match index {
                0 => (COMPONENT_CID, "Engine fixture"),
                1 => (CONTROLLER_CID, "Engine fixture controller"),
                _ => return kInvalidArgument,
            };
            // SAFETY: the host passes a writable PClassInfo.
            let info = &mut *info;
            info.cid = cid;
            for (dst, byte) in info.name.iter_mut().zip(name.bytes()) {
                *dst = byte as _;
            }
            kResultOk
        }
        unsafe fn createInstance(&self, cid: FIDString, _iid: FIDString, obj: *mut *mut c_void) -> tresult {
            // SAFETY: a class id is 16 bytes; `obj` is the host's out-pointer.
            let cid = *(cid as *const TUID);
            if cid == COMPONENT_CID {
                let mut component = FixtureComponent::new(None, false);
                component.controller_cid = Some(CONTROLLER_CID);
                component.latency.store(self.latency, Relaxed);
                component.output_level.store(self.output_level.to_bits(), Relaxed);
                component.inputs.store(self.inputs, Relaxed);
                let wrapper = ComWrapper::new(component);
                *obj = wrapper.to_com_ptr::<IComponent>().unwrap().into_raw().cast();
                *self.made.component.lock().unwrap() = Some(wrapper);
                kResultOk
            } else if cid == CONTROLLER_CID {
                let wrapper = ComWrapper::new(FixtureController::new(2));
                *obj = wrapper.to_com_ptr::<IEditController>().unwrap().into_raw().cast();
                *self.made.controller.lock().unwrap() = Some(wrapper);
                kResultOk
            } else {
                kResultFalse
            }
        }
    }

    /// A device rendering 256-frame blocks at `rate`, with a constant 0.1 as its input, paced near
    /// real time.
    fn device(rate: u32) -> TestDevice {
        TestDevice::start(rate, 256, Duration::from_millis(5), |_| 0.1)
    }

    fn rendered_near(device: &TestDevice, level: f32) -> bool {
        device.output.lock().unwrap().iter().rev().take(256).all(|&x| (x - level).abs() < 1e-3)
    }

    /// Load the fixture into `slot` through the engine-mode owner; the factory runs on the owner
    /// thread, so that thread is the component's main thread. The sink keeps every event.
    fn load(
        device: &TestDevice,
        slot: usize,
        latency: u32,
        output_level: f32,
    ) -> (EngineSlotHandle, Arc<Made>, Arc<Mutex<Vec<EngineSlotEvent>>>) {
        load_with_inputs(device, slot, latency, output_level, 0)
    }

    fn load_with_inputs(
        device: &TestDevice,
        slot: usize,
        latency: u32,
        output_level: f32,
        inputs: i32,
    ) -> (EngineSlotHandle, Arc<Made>, Arc<Mutex<Vec<EngineSlotEvent>>>) {
        let made = Arc::new(Made::default());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (factory_made, sink_seen) = (made.clone(), seen.clone());
        let handle = engine_slot::spawn(
            PluginFormat::Vst3,
            device.host().slot(slot),
            0,
            Arc::new(move |e| sink_seen.lock().unwrap().push(e)),
            move |ctx| {
                let id = super::super::super::super::scan::tuid_to_hex(&COMPONENT_CID);
                super::super::engine::run_with(ctx, &id, move || {
                    let factory = ComWrapper::new(FixtureFactory { made: factory_made, latency, output_level, inputs });
                    Ok((None, factory.to_com_ptr::<IPluginFactory>().ok_or("factory COM failed")?))
                })
            },
        )
        .expect("the fixture loads into the engine");
        (handle, made, seen)
    }

    fn installed(device: &TestDevice, slot: usize) -> Option<(SlotKind, lf_engine::grid::Frame)> {
        device.host().core.rt.lock().unwrap().engine.as_ref().unwrap().slot(slot)
    }

    fn send(device: &TestDevice, command: Command) {
        device.host().send(TimedCommand { frame: None, command }).unwrap();
    }

    /// The component handler the owner gave the controller: how the plugin reports to the host.
    fn plugin_side_handler(made: &Made) -> ComPtr<IComponentHandler> {
        made.controller().handler.lock().unwrap().clone().expect("the owner set a component handler")
    }

    #[test]
    fn a_loaded_slot_renders_in_the_engine_and_unloads_in_order() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let (handle, made, _) = load(&device, 1, 32, 0.25);
        let s = made.component();
        assert_eq!(handle.kind(), SlotKind::Instrument, "no audio input: an instrument");
        assert_eq!(handle.name(), "Engine fixture");
        assert!(wait_for(2000, || s.processes.load(Relaxed) > 8), "the engine processes the unit");
        assert!(
            wait_for(2000, || device.output.lock().unwrap().iter().rev().take(256).all(|&x| x > 0.2)),
            "its output reaches the device"
        );
        assert_eq!(installed(&device, 1), Some((SlotKind::Instrument, 32)), "installed with the latency it reported");
        assert_eq!(f64::from_bits(s.setup_rate.load(Relaxed)), 48_000.0, "set up at the engine's rate");
        assert_eq!(s.setup_max_frames.load(Relaxed), 4096, "with the engine's largest block");

        handle.unload().expect("the unit comes back");
        assert_eq!(installed(&device, 1), None, "the slot is empty");
        assert_eq!((s.starts.load(Relaxed), s.stops.load(Relaxed)), (1, 1));
        assert_eq!(s.deactivations.load(Relaxed), 1);
        let (stop, deactivate, terminate) =
            (s.stop_seq.load(Relaxed), s.deactivate_seq.load(Relaxed), s.terminate_seq.load(Relaxed));
        assert!(
            0 < stop && stop < deactivate && deactivate < terminate,
            "setProcessing(0) {stop} → setActive(0) {deactivate} → terminate {terminate}"
        );
        assert!(!s.contract_violation.load(Relaxed));
        assert_eq!(s.rt_calls_off_device.load(Relaxed), 0, "setProcessing and process ran on the device thread");
    }

    #[test]
    fn an_effect_slot_takes_the_live_input_and_its_wet_output_reaches_the_device() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let (handle, made, _) = load_with_inputs(&device, 1, 0, 0.0, 2);
        let s = made.component();
        assert_eq!(handle.kind(), SlotKind::Effect, "an audio input bus: an effect");
        assert!(wait_for(2000, || s.processes.load(Relaxed) > 8 && rendered_near(&device, 0.0)), "not live: silence");
        assert_eq!(installed(&device, 1), Some((SlotKind::Effect, 0)));
        send(&device, Command::SetSlotLive(1, true));
        assert!(
            wait_for(2000, || rendered_near(&device, 0.1)),
            "live: the device input, copied to both input channels and echoed, summed back to mono"
        );
        handle.unload().unwrap();
    }

    #[test]
    fn a_plugin_requested_restart_cycles_activation_on_the_owner_while_the_engine_renders() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let (handle, made, _) = load(&device, 0, 0, 0.0);
        let s = made.component();
        assert!(wait_for(2000, || s.processes.load(Relaxed) > 8));
        s.latency.store(64, Relaxed);
        let blocks_before = device.blocks.load(Relaxed);
        let processed_before = s.processes.load(Relaxed);

        // The plugin reports through its component handler, from a thread of its own.
        let reporter = plugin_side_handler(&made);
        std::thread::spawn(move || {
            // SAFETY: the handler is alive (held by the controller and this clone).
            assert_eq!(unsafe { reporter.restartComponent(RestartFlags_::kLatencyChanged) }, kResultOk);
        })
        .join()
        .unwrap();
        assert!(
            wait_for(2000, || s.activations.load(Relaxed) == 2
                && s.starts.load(Relaxed) == 2
                && s.processes.load(Relaxed) > processed_before + 8),
            "setActive(0) → setupProcessing → setActive(1) → reinstalled → processing again"
        );
        assert_eq!((s.stops.load(Relaxed), s.deactivations.load(Relaxed), s.setups.load(Relaxed)), (1, 1, 2));
        assert!(device.blocks.load(Relaxed) > blocks_before, "the device kept rendering through the restart");
        assert_eq!(
            device.host().core.counters.lock_misses.load(Relaxed),
            0,
            "the owner never held the engine while the device ran"
        );
        assert!(!s.contract_violation.load(Relaxed), "lifecycle on the owner, setProcessing/process off it");
        assert_eq!(s.rt_calls_off_device.load(Relaxed), 0, "setProcessing and process ran on the device thread");
        let owner = s.main_thread_calls.lock().unwrap().clone();
        assert_eq!(owner.len(), 5, "load: setup + activate; restart: deactivate + setup + activate");
        assert!(owner.iter().all(|t| *t == owner[0]) && owner[0] != std::thread::current().id());
        assert_eq!(installed(&device, 0), Some((SlotKind::Instrument, 64)), "reinstalled at the latency it reports now");
        handle.unload().unwrap();
    }

    #[test]
    fn a_note_sent_through_the_engine_reaches_the_plugin() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let (handle, made, _) = load(&device, 0, 0, 0.0);
        let s = made.component();
        assert!(wait_for(2000, || s.processes.load(Relaxed) > 4));
        send(&device, Command::SelectInstrument(NoteTarget::Slot(0)));
        send(&device, Command::NoteOn(62, 0.5));
        assert!(wait_for(2000, || s.note_ons.load(Relaxed) == 1), "the note-on reaches the plugin");
        assert_eq!(s.last_pitch.load(Relaxed), 62);
        send(&device, Command::NoteOff(62));
        assert!(wait_for(2000, || s.note_offs.load(Relaxed) == 1), "and its note-off");
        handle.unload().unwrap();
    }

    #[test]
    fn host_and_editor_param_changes_reach_both_halves_and_a_relist_reaches_the_caller() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let (handle, made, seen) = load(&device, 0, 0, 0.0);
        let s = made.component();
        assert!(handle.set_param(7, 0.1).is_err(), "an id the controller never listed");
        handle.set_param(1001, 0.3).unwrap();
        assert!(wait_for(2000, || s.param_points.load(Relaxed) == 1), "the processor gets the value");
        assert_eq!((s.last_param.load(Relaxed), f64::from_bits(s.last_param_value.load(Relaxed))), (1001, 0.3));
        let controller = made.controller();
        assert!(
            wait_for(2000, || controller.set_normalized.lock().unwrap().as_slice() == [(1001, 0.3)]),
            "and so does the edit controller"
        );

        // A knob moved in the plugin's editor: the processor hears it and the caller is told.
        // SAFETY: the handler is alive (held by the controller and this clone).
        assert_eq!(unsafe { plugin_side_handler(&made).performEdit(1000, 0.6) }, kResultOk);
        assert!(wait_for(2000, || s.param_points.load(Relaxed) == 2 && s.last_param.load(Relaxed) == 1000));
        assert_eq!(seen.lock().unwrap().as_slice(), [EngineSlotEvent::ParamChanged { id: 1000, value: 0.6 }]);

        // A preset loaded inside the plugin: the caller is told to list the params again.
        // SAFETY: as above.
        let reported = unsafe { plugin_side_handler(&made).restartComponent(RestartFlags_::kParamValuesChanged) };
        assert_eq!(reported, kResultOk);
        assert!(wait_for(2000, || seen.lock().unwrap().last() == Some(&EngineSlotEvent::ParamsChanged)));
        assert_eq!(s.activations.load(Relaxed), 1, "a re-list is not a restart");
        handle.unload().unwrap();
    }

    #[test]
    fn an_eviction_reactivates_the_plugin_at_the_new_rate() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let (handle, made, _) = load(&device, 1, 0, 0.0);
        let s = made.component();
        assert!(wait_for(2000, || s.processes.load(Relaxed) > 4));

        device.rebuild_at(44_100, 1024);
        assert!(
            wait_for(2000, || s.activations.load(Relaxed) == 2 && s.starts.load(Relaxed) == 2),
            "the owner took the unit back and reinstalled it"
        );
        assert_eq!(f64::from_bits(s.setup_rate.load(Relaxed)), 44_100.0, "set up again at the new rate");
        assert_eq!(s.setup_max_frames.load(Relaxed), 1024, "and the new engine's block");
        assert_eq!((s.stops.load(Relaxed), s.deactivations.load(Relaxed)), (1, 1));
        let processed = s.processes.load(Relaxed);
        assert!(wait_for(2000, || s.processes.load(Relaxed) > processed + 4), "the new engine processes it");
        assert!(installed(&device, 1).is_some());
        assert!(!s.contract_violation.load(Relaxed));
        handle.unload().unwrap();
        assert_eq!(s.deactivations.load(Relaxed), 2);
    }
}

/// The unit on its own: a call longer than the plugin's max frames goes in slices, each note lands
/// in its slice at its offset there, ring params go into the first, the outputs are summed to mono,
/// and nothing allocates once processing has started.
#[cfg(debug_assertions)]
#[test]
fn a_unit_slices_a_long_call_places_each_event_and_allocates_nothing() {
    use super::engine::Vst3Unit;
    use lf_engine::{SlotEvent, SlotEventKind, SlotProcessor};

    let fixture = ComWrapper::new(FixtureComponent::new(None, false));
    let s: &FixtureComponent = &fixture;
    s.output_level.store(0.5f32.to_bits(), Relaxed);
    let component = fixture.to_com_ptr::<IComponent>().unwrap();
    let processor = component.cast::<IAudioProcessor>().unwrap();
    let activation = unsafe { activate_component(&component, &processor, 48_000.0, 64) }.unwrap();
    let (mut params, ring) = RingBuffer::<PluginEvent>::new(8);
    let faults = Arc::new(AtomicU32::new(0));
    let unit = Vst3Unit::new(processor, activation, 64, ring, faults.clone()).unwrap();
    let (unit, runs) = std::thread::spawn(move || {
        let mut unit = unit;
        let (input, mut out) = ([0.0f32; 200], [0.0f32; 200]);
        unit.process(0, &input[..64], &[], &mut out[..64]); // starts processing: one call
        params.push(PluginEvent::Param { id: 100, value: 0.5 }).unwrap();
        let notes = [
            SlotEvent { offset: 10, kind: SlotEventKind::NoteOn { key: 60, velocity: 1.0 } },
            SlotEvent { offset: 150, kind: SlotEventKind::NoteOn { key: 64, velocity: 1.0 } },
        ];
        // A window the shared counter was reset in runs again (`rt_allocations`); count the runs.
        let mut runs = 0;
        let allocations = super::super::engine_slot::rt_allocations(|| {
            runs += 1;
            unit.process(200, &input, &notes, &mut out)
        });
        assert_eq!(allocations, 0, "process allocates nothing");
        assert!(out.iter().all(|&x| x == 0.5), "both channels at 0.5, summed to mono");
        unit.stop();
        (unit, runs)
    })
    .join()
    .unwrap();
    assert_eq!(s.processes.load(Relaxed), 1 + 4 * runs, "200 frames at 64 max: 64 + 64 + 64 + 8");
    assert_eq!(s.note_ons.load(Relaxed), 2 * runs);
    assert_eq!(
        (s.last_pitch.load(Relaxed), s.last_note_offset.load(Relaxed)),
        (64, 150 - 128),
        "the second note in the third slice"
    );
    assert_eq!(s.param_points.load(Relaxed), 1, "the ring's param once, in the first slice");
    assert_eq!(faults.load(Relaxed), 0);
    drop(unit);
    unsafe {
        assert_eq!(component.setActive(0), kResultOk);
    }
    assert!(!s.contract_violation.load(Relaxed));
}
