//! A VST3 component implemented in Rust through the real `IComponent` + `IAudioProcessor` vtables
//! (the `vst3` crate's `ComWrapper`), with counting lifecycle methods, and the host `MemStream`
//! tests. The engine-mode tests create the component through an in-process factory and load it with
//! `engine_slot` into a test device's engine, where the production `activate_component` activates it
//! and a plugin-requested restart cycles it on the owner. No DLL, audio device or GUI required.
use super::*;
use rtrb::RingBuffer;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};
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
    /// The component's state: what `getState` writes and `setState` replaces (the tone tests).
    state: Mutex<Vec<u8>>,
    /// `setProcessing(1)` answers `kResultFalse` (a plugin that refuses to start processing).
    reject_processing_start: AtomicBool,
    /// `setState` refuses whatever it is given.
    refuse_state: AtomicBool,
    /// `setState` applies what it is given, then refuses: a plugin that takes the fields it knows
    /// before it meets one it does not.
    half_apply_state: AtomicBool,
    set_states: AtomicUsize,
    /// A `setState` arrived while the component was active: a tone must go in before activation.
    state_set_while_active: AtomicBool,
}

impl FixtureComponent {
    fn new() -> Self {
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
            state: Mutex::new(Vec::new()),
            reject_processing_start: AtomicBool::new(false),
            refuse_state: AtomicBool::new(false),
            half_apply_state: AtomicBool::new(false),
            set_states: AtomicUsize::new(0),
            state_set_while_active: AtomicBool::new(false),
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
        } else {
            self.violate_if(!self.active.swap(false, Relaxed) || self.processing.load(Relaxed));
            self.tick(&self.deactivate_seq);
            self.deactivations.fetch_add(1, Relaxed);
        }
        kResultOk
    }
    /// Reads a length prefix, then that many bytes, as a plugin that framed its own state would.
    unsafe fn setState(&self, state: *mut IBStream) -> tresult {
        self.violate_if(!self.on_owner());
        self.state_set_while_active.fetch_or(self.active.load(Relaxed), Relaxed);
        let bytes = read_stream(state);
        let framed = bytes.get(..4).map(|len| u32::from_le_bytes(len.try_into().unwrap()) as usize);
        if self.half_apply_state.load(Relaxed) && framed.is_some() {
            *self.state.lock().unwrap() = bytes[4..].to_vec();
            return kResultFalse;
        }
        if self.refuse_state.load(Relaxed) || framed != Some(bytes.len() - 4) {
            return kResultFalse;
        }
        *self.state.lock().unwrap() = bytes[4..].to_vec();
        self.set_states.fetch_add(1, Relaxed);
        kResultOk
    }
    /// Writes a placeholder, the state, then seeks back to fill the length in, as JUCE's framing
    /// does: the host's stream must seek and tell.
    unsafe fn getState(&self, state: *mut IBStream) -> tresult {
        self.violate_if(!self.on_owner());
        let stream = ComRef::<IBStream>::from_raw(state).expect("a stream");
        let body = self.state.lock().unwrap().clone();
        let mut n = 0;
        let mut at: int64 = -1;
        let written = stream.write([0u8; 4].as_ptr() as *mut c_void, 4, &mut n) == kResultOk
            && stream.write(body.as_ptr() as *mut c_void, body.len() as int32, &mut n) == kResultOk
            && stream.tell(&mut at) == kResultOk
            && stream.seek(0, IStreamSeekMode_::kIBSeekSet, std::ptr::null_mut()) == kResultOk
            && stream.write((body.len() as u32).to_le_bytes().as_ptr() as *mut c_void, 4, &mut n) == kResultOk
            && stream.seek(at, IStreamSeekMode_::kIBSeekSet, std::ptr::null_mut()) == kResultOk;
        if written { kResultOk } else { kResultFalse }
    }
}

/// The host stream a VST3 tone goes through (`MemStream`): a read stops at the end, a write grows
/// it (a gap a seek left reads as zeros), seek and tell agree in every mode, and a bad argument is
/// refused rather than trusted.
#[test]
fn the_host_stream_reads_writes_seeks_and_tells() {
    use IStreamSeekMode_::{kIBSeekCur, kIBSeekEnd, kIBSeekSet};
    let stream = ComWrapper::new(MemStream::reading(b"abc"));
    let ptr = stream.to_com_ptr::<IBStream>().unwrap();
    // SAFETY: every pointer handed to the stream is valid for its call.
    unsafe {
        let (mut buf, mut n, mut at) = ([0u8; 8], 0, 0);
        assert_eq!(ptr.read(buf.as_mut_ptr().cast(), 8, &mut n), kResultOk);
        assert_eq!((n, &buf[..3]), (3, &b"abc"[..]));
        assert_eq!(ptr.read(buf.as_mut_ptr().cast(), 8, &mut n), kResultOk);
        assert_eq!(n, 0, "at the end");
        assert_eq!(ptr.seek(5, kIBSeekSet, &mut at), kResultOk);
        assert_eq!(at, 5);
        assert_eq!(ptr.write(b"xy".as_ptr() as *mut c_void, 2, &mut n), kResultOk);
        assert_eq!(n, 2);
        assert_eq!(stream.bytes(), b"abc\0\0xy");
        assert_eq!(ptr.tell(&mut at), kResultOk);
        assert_eq!(at, 7);
        assert_eq!(ptr.seek(-2, kIBSeekEnd, &mut at), kResultOk);
        assert_eq!(at, 5);
        assert_eq!(ptr.seek(1, kIBSeekCur, std::ptr::null_mut()), kResultOk);
        assert_eq!(ptr.read(buf.as_mut_ptr().cast(), 1, std::ptr::null_mut()), kResultOk);
        assert_eq!(buf[0], b'y');
        assert_eq!(ptr.seek(-1, kIBSeekSet, &mut at), kInvalidArgument, "before the start");
        assert_eq!(ptr.seek(0, 7, &mut at), kInvalidArgument, "no such mode");
        assert_eq!(ptr.read(buf.as_mut_ptr().cast(), -1, &mut n), kInvalidArgument);
        assert_eq!(ptr.write(std::ptr::null_mut(), 4, &mut n), kInvalidArgument);
        assert_eq!(ptr.tell(std::ptr::null_mut()), kInvalidArgument);
        let limit = super::super::super::tone::MAX_STATE_BYTES as int64;
        assert_eq!(ptr.seek(limit, kIBSeekSet, &mut at), kResultOk);
        assert_eq!(ptr.write(b"z".as_ptr() as *mut c_void, 1, &mut n), kResultFalse, "never past the tone limit");
    }
}

/// A read past the end reads nothing and leaves the cursor where it was: a plugin that seeks past
/// the end, reads, then tells or writes lands where it seeked, not back at the end.
#[test]
fn a_read_past_the_end_keeps_the_cursor() {
    use IStreamSeekMode_::kIBSeekSet;
    let stream = ComWrapper::new(MemStream::reading(b"abc"));
    let ptr = stream.to_com_ptr::<IBStream>().unwrap();
    // SAFETY: every pointer handed to the stream is valid for its call.
    unsafe {
        let (mut buf, mut n, mut at) = ([0u8; 4], -1, 0);
        assert_eq!(ptr.seek(5, kIBSeekSet, &mut at), kResultOk);
        assert_eq!(ptr.read(buf.as_mut_ptr().cast(), 4, &mut n), kResultOk);
        assert_eq!(n, 0, "nothing past the end");
        assert_eq!(ptr.tell(&mut at), kResultOk);
        assert_eq!(at, 5, "the cursor stays where the seek put it");
        assert_eq!(ptr.write(b"xy".as_ptr() as *mut c_void, 2, &mut n), kResultOk);
        assert_eq!(stream.bytes(), b"abc\0\0xy", "the write lands at the seeked position");
    }
}

/// Zero bytes with a null buffer is a valid call (a plugin writing or reading an empty field): it
/// moves nothing, grows nothing and never touches the null pointer.
#[test]
fn a_zero_length_transfer_with_a_null_buffer_is_a_no_op() {
    use IStreamSeekMode_::kIBSeekSet;
    let stream = ComWrapper::new(MemStream::reading(b"abc"));
    let ptr = stream.to_com_ptr::<IBStream>().unwrap();
    // SAFETY: the only null buffers passed come with a zero length; the counts are optional.
    unsafe {
        let (mut n, mut at) = (-1, -1);
        assert_eq!(ptr.read(std::ptr::null_mut(), 0, &mut n), kResultOk);
        assert_eq!(n, 0);
        assert_eq!(ptr.write(std::ptr::null_mut(), 0, &mut n), kResultOk);
        assert_eq!(n, 0);
        assert_eq!(ptr.read(std::ptr::null_mut(), 0, std::ptr::null_mut()), kResultOk);
        assert_eq!(ptr.tell(&mut at), kResultOk);
        assert_eq!(at, 0, "nothing moved");
        assert_eq!(ptr.seek(5, kIBSeekSet, &mut at), kResultOk);
        assert_eq!(ptr.write(std::ptr::null_mut(), 0, std::ptr::null_mut()), kResultOk);
        assert_eq!(stream.bytes(), b"abc", "an empty write past the end grows nothing");
    }
}

/// Everything left in a host stream, read in small pieces as a plugin reading field by field would.
pub(super) unsafe fn read_stream(stream: *mut IBStream) -> Vec<u8> {
    let stream = ComRef::<IBStream>::from_raw(stream).expect("a stream");
    let (mut out, mut chunk) = (Vec::new(), [0u8; 7]);
    loop {
        let mut n = 0;
        if stream.read(chunk.as_mut_ptr().cast(), chunk.len() as int32, &mut n) != kResultOk || n <= 0 {
            return out;
        }
        out.extend_from_slice(&chunk[..n as usize]);
    }
}

/// Write `bytes` into a host stream in one call.
pub(super) unsafe fn write_stream(stream: *mut IBStream, bytes: &[u8]) -> tresult {
    let stream = ComRef::<IBStream>::from_raw(stream).expect("a stream");
    let mut n = 0;
    stream.write(bytes.as_ptr() as *mut c_void, bytes.len() as int32, &mut n)
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
            if self.reject_processing_start.load(Relaxed) {
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

// ── engine mode: the same component as a unit inside a test device's engine (`engine_slot`) ─────

mod engine {
    use super::super::super::engine_slot::{self, EngineSlotEvent, EngineSlotHandle, PluginFormat};
    use super::super::super::super::state::ToneRestore;
    use super::super::super::super::tone::{self, TempDir, ToneBinding, ToneIdentity, SAVE_QUIET};
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
        /// How many components the factory made (a load that discards a refusing instance makes two).
        components: AtomicUsize,
    }

    /// What the fixture component's `setState` does with a tone.
    #[derive(Clone, Copy, PartialEq)]
    enum StateAnswer {
        Takes,
        /// Refuses it untouched.
        Refuses,
        /// Applies it, then refuses (`half_apply_state`).
        HalfAppliesThenRefuses,
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
        /// What every component it makes does with a `setState`.
        answer: StateAnswer,
        /// Every component it makes refuses `setProcessing(1)`.
        reject_start: bool,
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
                let mut component = FixtureComponent::new();
                component.controller_cid = Some(CONTROLLER_CID);
                component.latency.store(self.latency, Relaxed);
                component.output_level.store(self.output_level.to_bits(), Relaxed);
                component.inputs.store(self.inputs, Relaxed);
                component.refuse_state.store(self.answer == StateAnswer::Refuses, Relaxed);
                component.half_apply_state.store(self.answer == StateAnswer::HalfAppliesThenRefuses, Relaxed);
                component.reject_processing_start.store(self.reject_start, Relaxed);
                let wrapper = ComWrapper::new(component);
                *obj = wrapper.to_com_ptr::<IComponent>().unwrap().into_raw().cast();
                *self.made.component.lock().unwrap() = Some(wrapper);
                self.made.components.fetch_add(1, Relaxed);
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
        load_with_tone(device, slot, latency, output_level, inputs, None, StateAnswer::Takes)
    }

    fn load_with_tone(
        device: &TestDevice,
        slot: usize,
        latency: u32,
        output_level: f32,
        inputs: i32,
        tone: Option<ToneBinding>,
        answer: StateAnswer,
    ) -> (EngineSlotHandle, Arc<Made>, Arc<Mutex<Vec<EngineSlotEvent>>>) {
        load_factory(device, slot, tone, move |made| FixtureFactory {
            made,
            latency,
            output_level,
            inputs,
            answer,
            reject_start: false,
        })
    }

    /// Load through the engine-mode owner with the factory `make` builds (on the owner thread).
    fn load_factory(
        device: &TestDevice,
        slot: usize,
        tone: Option<ToneBinding>,
        make: impl FnOnce(Arc<Made>) -> FixtureFactory + Send + 'static,
    ) -> (EngineSlotHandle, Arc<Made>, Arc<Mutex<Vec<EngineSlotEvent>>>) {
        let made = Arc::new(Made::default());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (factory_made, sink_seen) = (made.clone(), seen.clone());
        let handle = engine_slot::spawn(
            PluginFormat::Vst3,
            device.host().slot(slot),
            0,
            Arc::new(move |e| sink_seen.lock().unwrap().push(e)),
            tone,
            move |ctx| {
                let id = super::super::super::super::scan::tuid_to_hex(&COMPONENT_CID);
                super::super::engine::run_with(ctx, &id, move || {
                    let factory = ComWrapper::new(make(factory_made));
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

    /// A restart into a layout the host refuses (0 output channels, `activate_component`'s channel
    /// check): the cycle fails after `setActive(0)`, the slot stays bypassed and silent with the
    /// component inactive, and the unload still tears it down in order.
    #[test]
    fn a_restart_into_a_refused_layout_leaves_the_slot_bypassed_and_unloadable() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let (handle, made, _) = load(&device, 0, 0, 0.25);
        let s = made.component();
        assert!(wait_for(2000, || s.processes.load(Relaxed) > 8 && !rendered_near(&device, 0.0)), "it plays");

        s.out_channels.store(0, Relaxed);
        let reporter = plugin_side_handler(&made);
        std::thread::spawn(move || {
            // SAFETY: the handler is alive (held by the controller and this clone).
            assert_eq!(unsafe { reporter.restartComponent(RestartFlags_::kIoChanged) }, kResultOk);
        })
        .join()
        .unwrap();
        assert!(
            wait_for(2000, || s.deactivations.load(Relaxed) == 1 && rendered_near(&device, 0.0)),
            "the cycle ran setActive(0) and the slot went silent"
        );
        // The cycle runs on the owner in one turn; past it, a reactivation would have shown by now.
        std::thread::sleep(Duration::from_millis(200));
        assert!(rendered_near(&device, 0.0), "still silent");
        assert_eq!(installed(&device, 0), None, "the refused unit stays out of the engine");
        assert_eq!(s.activations.load(Relaxed), 1, "setActive(1) never ran on the refused layout");
        assert_eq!(s.setups.load(Relaxed), 1, "refused at the bus read-back, before setupProcessing");
        assert!(!s.active.load(Relaxed), "the component is left inactive");
        assert!(!s.contract_violation.load(Relaxed), "the failed cycle kept the call sequence");

        handle.unload().expect("a bypassed slot still unloads");
        assert_eq!(s.deactivations.load(Relaxed), 1, "the unload sends no setActive(0) to an inactive component");
        assert!(s.terminate_seq.load(Relaxed) > 0, "terminated");
        assert!(!s.contract_violation.load(Relaxed), "and the unload kept the call sequence");
    }

    /// A plugin that refuses `setProcessing(1)` in the engine: asked once, never processed, silent,
    /// and never sent a `setProcessing(0)` it did not start; it still unloads in order.
    #[test]
    fn a_refused_processing_start_leaves_the_installed_slot_silent_and_unloadable() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let (handle, made, _) = load_refusing_start(&device, 0);
        let s = made.component();
        assert!(wait_for(2000, || s.starts.load(Relaxed) == 1), "the engine asks it to start");
        let blocks = device.blocks.load(Relaxed);
        assert!(wait_for(2000, || device.blocks.load(Relaxed) > blocks + 8), "the device keeps rendering");
        assert_eq!(s.starts.load(Relaxed), 1, "asked once, not every block");
        assert_eq!(s.processes.load(Relaxed), 0, "process never runs after the refusal");
        assert!(rendered_near(&device, 0.0), "the slot is silent");
        assert_eq!(installed(&device, 0), Some((SlotKind::Instrument, 0)), "it stays installed until reinstalled");

        handle.unload().expect("it unloads");
        assert_eq!(s.stops.load(Relaxed), 0, "a processor that never started is not stopped");
        assert_eq!(s.deactivations.load(Relaxed), 1);
        assert!(!s.contract_violation.load(Relaxed));
    }

    /// A full parameter queue: the set is refused and the edit controller never hears the value the
    /// processor did not get. A plugin that refuses to start keeps the unit from draining the ring.
    #[test]
    fn a_param_set_into_a_full_queue_is_refused_and_never_reaches_the_controller() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let (handle, made, _) = load_refusing_start(&device, 0);
        let s = made.component();
        assert!(wait_for(2000, || s.starts.load(Relaxed) == 1), "installed; the ring is never drained now");
        let capacity = super::super::super::EVENT_RING_CAP;
        for i in 0..capacity {
            handle.set_param(1001, i as f64 / capacity as f64).expect("the queue has room");
        }
        let err = handle.set_param(1001, 0.999).expect_err("the queue is full");
        assert!(err.contains("event queue is full"), "{err}");
        let controller = made.controller();
        assert!(
            wait_for(5000, || controller.set_normalized.lock().unwrap().len() == capacity),
            "the controller hears every value the queue took"
        );
        std::thread::sleep(Duration::from_millis(100)); // a few more owner turns
        let heard = controller.set_normalized.lock().unwrap().clone();
        assert_eq!(heard.len(), capacity, "and nothing more");
        assert!(heard.iter().all(|&(_, value)| value != 0.999), "the refused value never reaches it");
        handle.unload().unwrap();
    }

    /// `load` with a component that refuses `setProcessing(1)` (output 0.25, were it to play).
    fn load_refusing_start(
        device: &TestDevice,
        slot: usize,
    ) -> (EngineSlotHandle, Arc<Made>, Arc<Mutex<Vec<EngineSlotEvent>>>) {
        load_factory(device, slot, None, |made| FixtureFactory {
            made,
            latency: 0,
            output_level: 0.25,
            inputs: 0,
            answer: StateAnswer::Takes,
            reject_start: true,
        })
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

    fn identity() -> ToneIdentity {
        ToneIdentity { format: "vst3".into(), path: r"C:\fixture.vst3".into(), id: "engine-fixture".into() }
    }

    /// The fixture's tone as the store holds it: its component and controller states.
    fn stored(dir: &TempDir) -> Option<(Vec<u8>, Vec<u8>)> {
        let tone = dir.binding(0, identity()).store.load(0, &identity()).unwrap()?;
        let (component, controller) = tone::decode_vst3(&tone.state).unwrap();
        Some((component.to_vec(), controller.to_vec()))
    }

    /// A component state as the fixture frames it: a u32 length, then the bytes.
    fn framed(body: &[u8]) -> Vec<u8> {
        [&(body.len() as u32).to_le_bytes()[..], body].concat()
    }

    fn store_tone(dir: &TempDir, component: &[u8], controller: &[u8]) {
        let t = tone::Tone { identity: identity(), name: "Engine fixture".into(), state: tone::encode_vst3(component, controller) };
        dir.binding(0, identity()).store.save_encoded(0, &tone::encode(&t)).unwrap();
    }

    #[test]
    fn a_stored_tone_reaches_both_halves_before_activation_and_saves_follow_changes() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let dir = TempDir::new("vst3-engine");
        store_tone(&dir, &framed(b"component v"), b"controller v");
        let (handle, made, _) = load_with_tone(&device, 0, 0, 0.0, 0, Some(dir.binding(0, identity())), StateAnswer::Takes);
        let (s, controller) = (made.component(), made.controller());
        assert_eq!(handle.tone(), Some(ToneRestore::Restored));
        assert_eq!(*s.state.lock().unwrap(), b"component v", "the component read the stream it was given");
        assert!(!s.state_set_while_active.load(Relaxed), "the tone went in before setActive(1)");
        assert_eq!(
            *controller.state_calls.lock().unwrap(),
            [("setComponentState", b"\x0b\0\0\0component v".to_vec()), ("setState", b"controller v".to_vec())],
            "the controller hears the component's state from its start, then its own"
        );
        assert!(wait_for(2000, || s.processes.load(Relaxed) > 4), "and the plugin runs");

        // An export takes the tone fresh, through a stream that had to seek and tell.
        *s.state.lock().unwrap() = b"component w".to_vec();
        *controller.state.lock().unwrap() = b"controller w".to_vec();
        let file = tone::decode(&handle.take_tone().unwrap()).unwrap();
        assert_eq!(file.identity, identity());
        let w = framed(b"component w");
        assert_eq!(tone::decode_vst3(&file.state).unwrap(), (&w[..], &b"controller w"[..]));
        assert_eq!(stored(&dir), Some((w, b"controller w".to_vec())), "and it lands in the store");

        // A host parameter set is a change: saved once it has been quiet for SAVE_QUIET.
        *s.state.lock().unwrap() = b"component x".to_vec();
        handle.set_param(1001, 0.3).unwrap();
        let wait = (SAVE_QUIET.as_millis() * 2 + 1000) as u64;
        assert!(
            wait_for(wait, || stored(&dir).is_some_and(|(c, _)| c.ends_with(b"component x"))),
            "debounced save after a host set"
        );

        // An editor's performEdit is a change too; an unload saves it at once.
        *s.state.lock().unwrap() = b"component y".to_vec();
        // SAFETY: the handler is alive (held by the controller and this clone).
        assert_eq!(unsafe { plugin_side_handler(&made).performEdit(1000, 0.6) }, kResultOk);
        handle.unload().unwrap();
        assert!(stored(&dir).is_some_and(|(c, _)| c.ends_with(b"component y")), "saved before the teardown");
        assert!(!s.contract_violation.load(Relaxed), "state calls on the owner thread");
    }

    #[test]
    fn a_tone_the_plugin_refuses_or_a_corrupt_file_loads_at_the_defaults() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let dir = TempDir::new("vst3-refused");
        store_tone(&dir, &framed(b"component v"), b"");
        let kept = Some((framed(b"component v"), Vec::new()));
        let (handle, made, _) = load_with_tone(&device, 0, 0, 0.0, 0, Some(dir.binding(0, identity())), StateAnswer::Refuses);
        let s = made.component();
        assert_eq!(handle.tone(), Some(ToneRestore::Failed), "refused, reported");
        assert!(s.state.lock().unwrap().is_empty(), "the component kept its defaults");
        assert!(wait_for(2000, || s.processes.load(Relaxed) > 4), "and the load went on");
        // The app's exit: a save asked of every slot, then the unload. Neither writes the defaults over
        // the tone the plugin refused (a reinstalled version of it may take it again).
        let exit = handle.start_tone_save().unwrap().recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
        let exit = tone::decode(&exit).unwrap();
        let (component, _) = tone::decode_vst3(&exit.state).unwrap();
        assert_eq!(component, &framed(b"")[..], "the save still hands back what plays: the defaults");
        assert_eq!(stored(&dir), kept, "the exit's save leaves the stored tone alone");
        handle.unload().unwrap();
        assert_eq!(stored(&dir), kept, "so does the unload");
        // A change the player makes is what the defaults replace it for.
        let (handle, _, _) = load_with_tone(&device, 0, 0, 0.0, 0, Some(dir.binding(0, identity())), StateAnswer::Refuses);
        handle.set_param(1001, 0.3).unwrap();
        handle.unload().unwrap();
        assert_eq!(stored(&dir), Some((framed(b""), Vec::new())), "a change is saved as usual");
        store_tone(&dir, &framed(b"component v"), b"");

        let path = dir.0.join(identity().file_name(0));
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() - 3]).unwrap();
        let (handle, made, _) = load_with_tone(&device, 1, 0, 0.0, 0, Some(dir.binding(0, identity())), StateAnswer::Takes);
        assert_eq!(handle.tone(), Some(ToneRestore::Failed), "a truncated file is no tone");
        assert_eq!(made.component().set_states.load(Relaxed), 0, "and never reaches the plugin");
        handle.unload().unwrap();
    }

    #[test]
    fn a_tone_the_plugin_half_takes_then_refuses_leaves_a_fresh_instance_at_its_defaults() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let dir = TempDir::new("vst3-half");
        store_tone(&dir, &framed(b"component v"), b"controller v");
        let answer = StateAnswer::HalfAppliesThenRefuses;
        let (handle, made, _) = load_with_tone(&device, 0, 0, 0.0, 0, Some(dir.binding(0, identity())), answer);
        assert_eq!(handle.tone(), Some(ToneRestore::Failed), "refused, reported");
        assert_eq!(made.components.load(Relaxed), 2, "the instance that took part of the tone was discarded");
        let s = made.component();
        assert!(s.state.lock().unwrap().is_empty(), "the running component is at its true defaults");
        assert!(made.controller().state_calls.lock().unwrap().is_empty(), "and so is its controller");
        assert!(wait_for(2000, || s.processes.load(Relaxed) > 4), "and it runs");
        handle.unload().unwrap();
        assert_eq!(stored(&dir), Some((framed(b"component v"), b"controller v".to_vec())), "the stored tone stays");
        assert!(!s.contract_violation.load(Relaxed));
    }

    #[test]
    fn a_load_from_before_an_import_never_writes_over_it() {
        let _one = engine_slot::one_engine_test_at_a_time();
        let device = device(48_000);
        let dir = TempDir::new("vst3-import");
        let (handle, made, _) = load_with_tone(&device, 0, 0, 0.0, 0, Some(dir.binding(0, identity())), StateAnswer::Takes);
        assert_eq!(handle.tone(), None, "nothing stored yet");
        let t = tone::Tone { identity: identity(), name: "Engine fixture".into(), state: tone::encode_vst3(b"imported", b"") };
        dir.store().import(0, &tone::encode(&t), &identity()).unwrap();
        *made.component().state.lock().unwrap() = b"this load".to_vec();
        handle.set_param(1001, 0.3).unwrap();
        let file = tone::decode(&handle.take_tone().unwrap()).unwrap();
        assert!(tone::decode_vst3(&file.state).unwrap().0.ends_with(b"this load"), "an export still gets this load's state");
        handle.unload().unwrap();
        assert_eq!(stored(&dir), Some((b"imported".to_vec(), Vec::new())), "the store keeps the import");
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

/// The unit on its own, with a plugin that refuses `setProcessing(1)`: the refusal latches
/// `FAULT_START` (what the owner reports), the unit stays silent without asking again or entering
/// `process`, and its stop sends no `setProcessing(0)`.
#[test]
fn a_unit_whose_plugin_refuses_to_start_latches_the_fault_and_stays_silent() {
    use super::engine::Vst3Unit;
    use lf_engine::SlotProcessor;

    let fixture = ComWrapper::new(FixtureComponent::new());
    let s: &FixtureComponent = &fixture;
    s.output_level.store(0.5f32.to_bits(), Relaxed);
    s.reject_processing_start.store(true, Relaxed);
    let component = fixture.to_com_ptr::<IComponent>().unwrap();
    let processor = component.cast::<IAudioProcessor>().unwrap();
    let activation = unsafe { activate_component(&component, &processor, 48_000.0, 64) }.unwrap();
    let (_params, ring) = RingBuffer::<PluginEvent>::new(8);
    let faults = Arc::new(AtomicU32::new(0));
    let unit = Vst3Unit::new(processor, activation, 64, ring, faults.clone()).unwrap();
    let unit = std::thread::spawn(move || {
        let mut unit = unit;
        let (input, mut out) = ([0.0f32; 64], [1.0f32; 64]);
        for frame in 0..3 {
            unit.process(frame * 64, &input, &[], &mut out);
            assert!(out.iter().all(|&x| x == 0.0), "silent");
        }
        unit.stop();
        unit
    })
    .join()
    .unwrap();
    assert_eq!(faults.load(Relaxed), super::super::engine_slot::FAULT_START, "the refusal is latched");
    assert_eq!(s.starts.load(Relaxed), 1, "asked once");
    assert_eq!(s.processes.load(Relaxed), 0);
    assert_eq!(s.stops.load(Relaxed), 0, "never started, never stopped");
    drop(unit);
    unsafe {
        assert_eq!(component.setActive(0), kResultOk);
    }
    assert!(!s.contract_violation.load(Relaxed));
}

/// The unit on its own: a call longer than the plugin's max frames goes in slices, each note lands
/// in its slice at its offset there, ring params go into the first, the outputs are summed to mono,
/// and nothing allocates once processing has started.
#[cfg(debug_assertions)]
#[test]
fn a_unit_slices_a_long_call_places_each_event_and_allocates_nothing() {
    use super::engine::Vst3Unit;
    use lf_engine::{SlotEvent, SlotEventKind, SlotProcessor};

    let fixture = ComWrapper::new(FixtureComponent::new());
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
