//! LF Test Gain (VST3): a separated component (`IComponent` + `IAudioProcessor`) and edit
//! controller, stereo in, stereo out, `out = in × GAIN`. One parameter, GAIN (id 7), normalized
//! 0..1 for a plain 0..2, default 0.25 (×0.5); the component's state is the normalized value as 8
//! little-endian bytes, which the controller reads back through `setComponentState`.
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use vst3::Steinberg::Vst::{
    BusDirections_, BusInfo, IAudioProcessor, IAudioProcessor_iid, IAudioProcessorTrait, IComponent, IComponentHandler, IComponentTrait,
    IEditController, IEditControllerTrait, IParamValueQueue, IParamValueQueueTrait, IParameterChanges,
    IParameterChangesTrait, IoMode, MediaType, MediaTypes_, ParamID, ParamValue, ParameterInfo, ProcessData,
    ProcessSetup, RoutingInfo, SpeakerArr, SpeakerArrangement, String128, SymbolicSampleSizes_, TChar,
};
use vst3::Steinberg::{
    int32, kInvalidArgument, kResultFalse, kResultOk, tresult, uint32, FIDString, FUnknown, IBStream, IBStreamTrait,
    IPlugView, IPluginBaseTrait, IPluginFactory, IPluginFactoryTrait, PClassInfo, PFactoryInfo, TBool, TUID,
};
use vst3::{Class, ComRef, ComWrapper};

const GAIN_ID: ParamID = 7;
const GAIN_DEFAULT: f64 = 0.25;
const COMPONENT_CID: [u8; 16] = *b"LFTestGainProc01";
const CONTROLLER_CID: [u8; 16] = *b"LFTestGainCtrl01";

fn tuid(bytes: [u8; 16]) -> TUID {
    bytes.map(|b| b as _)
}

/// ASCII into a C string field (`char8`) or a UTF-16 one (`TChar`), NUL-terminated.
trait Ascii: Copy {
    fn from_ascii(byte: u8) -> Self;
}
impl Ascii for i8 {
    fn from_ascii(byte: u8) -> Self {
        byte as i8
    }
}
impl Ascii for u16 {
    fn from_ascii(byte: u8) -> Self {
        u16::from(byte)
    }
}
fn copy_ascii<T: Ascii>(dst: &mut [T], src: &str) {
    for (d, s) in dst.iter_mut().zip(src.bytes().chain(std::iter::once(0))) {
        *d = T::from_ascii(s);
    }
}

unsafe fn read_f64(stream: *mut IBStream) -> Option<f64> {
    let stream = unsafe { ComRef::<IBStream>::from_raw(stream)? };
    let mut bytes = [0u8; 8];
    let mut got = 0;
    let ok = unsafe { stream.read(bytes.as_mut_ptr().cast(), 8, &mut got) } == kResultOk;
    (ok && got == 8).then(|| f64::from_le_bytes(bytes))
}

unsafe fn write_f64(stream: *mut IBStream, value: f64) -> tresult {
    let Some(stream) = (unsafe { ComRef::<IBStream>::from_raw(stream) }) else { return kInvalidArgument };
    let bytes = value.to_le_bytes();
    let mut put = 0;
    let ok = unsafe { stream.write(bytes.as_ptr() as *mut c_void, 8, &mut put) } == kResultOk;
    if ok && put == 8 { kResultOk } else { kResultFalse }
}

struct Component {
    gain: AtomicU64,
}

impl Class for Component {
    type Interfaces = (IComponent, IAudioProcessor);
}

impl IPluginBaseTrait for Component {
    unsafe fn initialize(&self, _context: *mut FUnknown) -> tresult {
        kResultOk
    }
    unsafe fn terminate(&self) -> tresult {
        kResultOk
    }
}

impl IComponentTrait for Component {
    unsafe fn getControllerClassId(&self, class_id: *mut TUID) -> tresult {
        unsafe { *class_id = tuid(CONTROLLER_CID) };
        kResultOk
    }
    unsafe fn setIoMode(&self, _mode: IoMode) -> tresult {
        kResultOk
    }
    unsafe fn getBusCount(&self, r#type: MediaType, _dir: int32) -> int32 {
        (r#type == MediaTypes_::kAudio as MediaType) as int32
    }
    unsafe fn getBusInfo(&self, r#type: MediaType, dir: int32, index: int32, bus: *mut BusInfo) -> tresult {
        if r#type != MediaTypes_::kAudio as MediaType || index != 0 {
            return kInvalidArgument;
        }
        let bus = unsafe { &mut *bus };
        bus.mediaType = r#type;
        bus.direction = dir;
        bus.channelCount = 2;
        copy_ascii(&mut bus.name, if dir == BusDirections_::kInput as int32 { "in" } else { "out" });
        bus.busType = 0;
        bus.flags = 1;
        kResultOk
    }
    unsafe fn getRoutingInfo(&self, _in: *mut RoutingInfo, _out: *mut RoutingInfo) -> tresult {
        kResultFalse
    }
    unsafe fn activateBus(&self, _t: MediaType, _d: int32, _i: int32, _s: TBool) -> tresult {
        kResultOk
    }
    unsafe fn setActive(&self, _state: TBool) -> tresult {
        kResultOk
    }
    unsafe fn setState(&self, state: *mut IBStream) -> tresult {
        match unsafe { read_f64(state) } {
            Some(v) => {
                self.gain.store(v.clamp(0.0, 1.0).to_bits(), Relaxed);
                kResultOk
            }
            None => kResultFalse,
        }
    }
    unsafe fn getState(&self, state: *mut IBStream) -> tresult {
        unsafe { write_f64(state, f64::from_bits(self.gain.load(Relaxed))) }
    }
}

impl IAudioProcessorTrait for Component {
    unsafe fn setBusArrangements(&self, _i: *mut SpeakerArrangement, _ni: int32, _o: *mut SpeakerArrangement, _no: int32) -> tresult {
        kResultOk
    }
    unsafe fn getBusArrangement(&self, _dir: int32, _index: int32, arr: *mut SpeakerArrangement) -> tresult {
        unsafe { *arr = SpeakerArr::kStereo };
        kResultOk
    }
    unsafe fn canProcessSampleSize(&self, size: int32) -> tresult {
        if size == SymbolicSampleSizes_::kSample32 as int32 { kResultOk } else { kResultFalse }
    }
    unsafe fn getLatencySamples(&self) -> uint32 {
        0
    }
    unsafe fn setupProcessing(&self, _setup: *mut ProcessSetup) -> tresult {
        kResultOk
    }
    unsafe fn setProcessing(&self, _state: TBool) -> tresult {
        kResultOk
    }
    unsafe fn process(&self, data: *mut ProcessData) -> tresult {
        let data = unsafe { &*data };
        if let Some(changes) = unsafe { ComRef::<IParameterChanges>::from_raw(data.inputParameterChanges) } {
            for i in 0..unsafe { changes.getParameterCount() } {
                let Some(queue) = (unsafe { ComRef::<IParamValueQueue>::from_raw(changes.getParameterData(i)) }) else { continue };
                let points = unsafe { queue.getPointCount() };
                let (mut offset, mut value) = (0, 0.0);
                if unsafe { queue.getParameterId() } == GAIN_ID && points > 0 && unsafe { queue.getPoint(points - 1, &mut offset, &mut value) } == kResultOk {
                    self.gain.store(value.clamp(0.0, 1.0).to_bits(), Relaxed);
                }
            }
        }
        if data.numInputs < 1 || data.numOutputs < 1 || data.inputs.is_null() || data.outputs.is_null() {
            return kResultOk;
        }
        let gain = (f64::from_bits(self.gain.load(Relaxed)) * 2.0) as f32;
        let (input, output) = unsafe { (&*data.inputs, &*data.outputs) };
        let frames = data.numSamples as usize;
        for ch in 0..output.numChannels.min(2) as usize {
            let src_ch = ch.min(input.numChannels.max(1) as usize - 1);
            unsafe {
                let src = std::slice::from_raw_parts(*input.__field0.channelBuffers32.add(src_ch), frames);
                let dst = std::slice::from_raw_parts_mut(*output.__field0.channelBuffers32.add(ch), frames);
                for (d, s) in dst.iter_mut().zip(src) {
                    *d = s * gain;
                }
            }
        }
        kResultOk
    }
    unsafe fn getTailSamples(&self) -> uint32 {
        0
    }
}

struct Controller {
    gain: AtomicU64,
}

impl Class for Controller {
    type Interfaces = (IEditController,);
}

impl IPluginBaseTrait for Controller {
    unsafe fn initialize(&self, _context: *mut FUnknown) -> tresult {
        kResultOk
    }
    unsafe fn terminate(&self) -> tresult {
        kResultOk
    }
}

impl IEditControllerTrait for Controller {
    unsafe fn setComponentState(&self, state: *mut IBStream) -> tresult {
        if let Some(v) = unsafe { read_f64(state) } {
            self.gain.store(v.clamp(0.0, 1.0).to_bits(), Relaxed);
        }
        kResultOk
    }
    unsafe fn setState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }
    unsafe fn getState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }
    unsafe fn getParameterCount(&self) -> int32 {
        1
    }
    unsafe fn getParameterInfo(&self, index: int32, info: *mut ParameterInfo) -> tresult {
        if index != 0 {
            return kInvalidArgument;
        }
        let info = unsafe { &mut *info };
        info.id = GAIN_ID;
        copy_ascii(&mut info.title, "Gain");
        copy_ascii(&mut info.shortTitle, "Gain");
        copy_ascii(&mut info.units, "x");
        info.stepCount = 0;
        info.defaultNormalizedValue = GAIN_DEFAULT;
        info.unitId = 0;
        info.flags = 1; // kCanAutomate
        kResultOk
    }
    unsafe fn getParamStringByValue(&self, _id: ParamID, value: ParamValue, string: *mut String128) -> tresult {
        copy_ascii(unsafe { &mut *string }, &format!("{:.3}", value * 2.0));
        kResultOk
    }
    unsafe fn getParamValueByString(&self, _id: ParamID, _string: *mut TChar, _value: *mut ParamValue) -> tresult {
        kResultFalse
    }
    unsafe fn normalizedParamToPlain(&self, _id: ParamID, value: ParamValue) -> ParamValue {
        value * 2.0
    }
    unsafe fn plainParamToNormalized(&self, _id: ParamID, value: ParamValue) -> ParamValue {
        value / 2.0
    }
    unsafe fn getParamNormalized(&self, id: ParamID) -> ParamValue {
        if id == GAIN_ID { f64::from_bits(self.gain.load(Relaxed)) } else { 0.0 }
    }
    unsafe fn setParamNormalized(&self, id: ParamID, value: ParamValue) -> tresult {
        if id != GAIN_ID {
            return kInvalidArgument;
        }
        self.gain.store(value.clamp(0.0, 1.0).to_bits(), Relaxed);
        kResultOk
    }
    unsafe fn setComponentHandler(&self, _handler: *mut IComponentHandler) -> tresult {
        kResultOk
    }
    unsafe fn createView(&self, _name: FIDString) -> *mut IPlugView {
        std::ptr::null_mut()
    }
}

struct Factory;

impl Class for Factory {
    type Interfaces = (IPluginFactory,);
}

impl IPluginFactoryTrait for Factory {
    unsafe fn getFactoryInfo(&self, info: *mut PFactoryInfo) -> tresult {
        let info = unsafe { &mut *info };
        copy_ascii(&mut info.vendor, "BleepLoop");
        copy_ascii(&mut info.url, "");
        copy_ascii(&mut info.email, "");
        info.flags = 16; // kUnicode
        kResultOk
    }
    unsafe fn countClasses(&self) -> int32 {
        2
    }
    unsafe fn getClassInfo(&self, index: int32, info: *mut PClassInfo) -> tresult {
        let (cid, category, name) = match index {
            0 => (COMPONENT_CID, "Audio Module Class", "LF Test Gain"),
            1 => (CONTROLLER_CID, "Component Controller Class", "LF Test Gain Controller"),
            _ => return kInvalidArgument,
        };
        let info = unsafe { &mut *info };
        info.cid = tuid(cid);
        info.cardinality = 0x7FFF_FFFF;
        copy_ascii(&mut info.category, category);
        copy_ascii(&mut info.name, name);
        kResultOk
    }
    unsafe fn createInstance(&self, cid: FIDString, iid: FIDString, obj: *mut *mut c_void) -> tresult {
        let cid = unsafe { *(cid as *const TUID) };
        let iid = unsafe { *(iid as *const TUID) };
        let raw = if cid == tuid(COMPONENT_CID) {
            let w = ComWrapper::new(Component { gain: AtomicU64::new(GAIN_DEFAULT.to_bits()) });
            if iid == IAudioProcessor_iid {
                w.to_com_ptr::<IAudioProcessor>().map(|p| p.into_raw().cast())
            } else {
                w.to_com_ptr::<IComponent>().map(|p| p.into_raw().cast())
            }
        } else if cid == tuid(CONTROLLER_CID) {
            ComWrapper::new(Controller { gain: AtomicU64::new(GAIN_DEFAULT.to_bits()) })
                .to_com_ptr::<IEditController>()
                .map(|p| p.into_raw().cast())
        } else {
            None
        };
        match raw {
            Some(raw) => {
                unsafe { *obj = raw };
                kResultOk
            }
            None => {
                unsafe { *obj = std::ptr::null_mut() };
                kInvalidArgument
            }
        }
    }
}

#[no_mangle]
pub extern "C" fn GetPluginFactory() -> *mut IPluginFactory {
    ComWrapper::new(Factory).to_com_ptr::<IPluginFactory>().map_or(std::ptr::null_mut(), |p| p.into_raw())
}

#[no_mangle]
pub extern "C" fn ModuleEntry(_handle: *mut c_void) -> bool {
    true
}

#[no_mangle]
pub extern "C" fn ModuleExit() -> bool {
    true
}
