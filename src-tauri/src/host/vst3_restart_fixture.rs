//! A VST3 component implemented in Rust through the real `IComponent` + `IAudioProcessor` vtables
//! (the `vst3` crate's `ComWrapper`), with counting lifecycle methods, activated by the production
//! `activate_component` and driven by the production RT producer (`vst3_producer_loop`) against a
//! plain heap ring in place of the WebView2 SharedBuffer. Proves the plugin-initiated restart
//! cycle: `setProcessing(0)` on the RT thread → `setActive(0)` → `setupProcessing` → `setActive(1)`
//! on the owner → RT resumed, with the layout re-queried and nothing dropped. No DLL, audio device
//! or GUI required.
use super::super::super::transport::HOP1_HEADER_BYTES;
use super::*;
use rtrb::RingBuffer;
use std::sync::atomic::AtomicUsize;
use std::thread::ThreadId;
use vst3::Steinberg::Vst::{IoMode, MediaType, RoutingInfo, SpeakerArrangement};
use vst3::Steinberg::{IBStream, TBool};

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
    main_thread_calls: Mutex<Vec<ThreadId>>,
}

impl FixtureComponent {
    fn new(raise_on_activate: Option<Arc<RestartFlags>>) -> Self {
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
            main_thread_calls: Mutex::new(Vec::new()),
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
        kResultOk
    }
}

impl IComponentTrait for FixtureComponent {
    unsafe fn getControllerClassId(&self, _class_id: *mut TUID) -> tresult {
        kResultFalse
    }
    unsafe fn setIoMode(&self, _mode: IoMode) -> tresult {
        kResultOk
    }
    unsafe fn getBusCount(&self, r#type: MediaType, dir: int32) -> int32 {
        let audio = r#type == MediaTypes_::kAudio as i32;
        let event = r#type == MediaTypes_::kEvent as i32;
        let input = dir == BusDirections_::kInput as i32;
        match (audio, event, input) {
            (true, _, false) => 1, // one audio output bus (a synth: no audio input)
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
        if r#type != MediaTypes_::kAudio as i32 || dir != BusDirections_::kOutput as i32 || index != 0
        {
            return kInvalidArgument;
        }
        // SAFETY: the host passes a valid, zeroed BusInfo.
        let bi = &mut *bus;
        bi.mediaType = r#type;
        bi.direction = dir;
        bi.channelCount = self.out_channels.load(Relaxed);
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
    unsafe fn setupProcessing(&self, _setup: *mut ProcessSetup) -> tresult {
        self.violate_if(!self.on_owner() || self.active.load(Relaxed));
        self.main_thread_calls.lock().unwrap().push(std::thread::current().id());
        self.setups.fetch_add(1, Relaxed);
        kResultOk
    }
    unsafe fn setProcessing(&self, state: TBool) -> tresult {
        self.violate_if(self.on_owner() || !self.active.load(Relaxed));
        if state != 0 {
            self.violate_if(self.processing.swap(true, Relaxed));
            self.starts.fetch_add(1, Relaxed);
        } else {
            self.violate_if(!self.processing.swap(false, Relaxed));
            self.stops.fetch_add(1, Relaxed);
        }
        kResultOk
    }
    unsafe fn process(&self, _data: *mut ProcessData) -> tresult {
        self.violate_if(
            self.on_owner() || !self.active.load(Relaxed) || !self.processing.load(Relaxed),
        );
        self.processes.fetch_add(1, Relaxed);
        kResultOk // renders silence: the host zeroes the rows before every call
    }
    unsafe fn getTailSamples(&self) -> uint32 {
        0
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
    let fixture = ComWrapper::new(FixtureComponent::new(Some(restart.clone())));
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

    // Stand in for the JS drain: keep the header read cursor ([1]) caught up with the write cursor
    // ([0]) until the producer is well past one ring capacity, so a restart that forgot the cursor
    // would wrap `used` and drop every block (the 2026-09-10 CLAP runtime finding).
    // SAFETY: the header words are 4-byte aligned; the RT thread is the sole writer of [0] and the
    // sole reader of [1], mirroring the WebView2 contract.
    let write_idx = unsafe { AtomicU32::from_ptr(ring.as_mut_ptr()) };
    let read_idx = unsafe { AtomicU32::from_ptr(ring.as_mut_ptr().add(1)) };
    assert!(
        wait_for(4000, || {
            let w = write_idx.load(Acquire);
            read_idx.store(w, Release);
            w > CAP_FRAMES * 2
        }),
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
        wait_for(2000, || {
            let w = write_idx.load(Acquire);
            read_idx.store(w, Release);
            w > written_before_restart + CAP_FRAMES
        }),
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
    drop(ring);
}

#[test]
fn a_failed_reactivation_leaves_the_slot_silent_but_serviceable() {
    const CAP_FRAMES: u32 = 1024;
    const MAX_FRAMES: u32 = 512;
    const RATE: f64 = 48_000.0;

    let restart = Arc::new(RestartFlags::default());
    let fixture = ComWrapper::new(FixtureComponent::new(None));
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
