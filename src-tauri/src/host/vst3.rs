use super::{
    checked_plugin_channels, checked_plugin_params, publish_param_ids, sum_to_mono, OwnerRequest,
    PluginEvent, MAX_EVENTS_PER_BLOCK,
};
use super::super::editor_window::{
    client_size, create_host_window, drain_after_editor_teardown, pump_thread_messages,
    set_client_size, show_host_window_front, wait_for_input, HostWindow,
};
use super::super::state::ParamDesc;

use std::cell::{Cell, RefCell, UnsafeCell};
use std::collections::HashMap;
use std::ffi::{c_void, CStr, CString};
use std::os::windows::ffi::OsStrExt;
use std::rc::Rc;
use std::sync::atomic::{
    AtomicI32, AtomicIsize, AtomicU32,
    Ordering::{Acquire, Relaxed, Release},
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rtrb::{Consumer, Producer};

use vst3::Steinberg::Vst::IAttributeList_::AttrID;
use vst3::Steinberg::Vst::{
    AudioBusBuffers, AudioBusBuffers__type0, BusDirections_, BusInfo, Event,
    Event__type0, IAttributeList, IAttributeListTrait, IAudioProcessor, IAudioProcessorTrait,
    IComponent, IComponentHandler, IComponentHandlerTrait, IComponentTrait, IConnectionPoint,
    IConnectionPointTrait, IEditController, IEditController_iid, IEditControllerTrait,
    IEventList, IEventListTrait, IHostApplication, IHostApplicationTrait, IMessage,
    IMessage_iid, IMessageTrait, IParamValueQueue, IParamValueQueueTrait, IParameterChanges,
    IParameterChangesTrait, MediaTypes_, NoteOffEvent, NoteOnEvent, ParamID, ParameterInfo,
    ParamValue, ProcessData, ProcessModes_,
    ProcessSetup, RestartFlags_, SpeakerArr, String128, SymbolicSampleSizes_, TChar, ViewType,
};
use vst3::Steinberg::IBStream_::IStreamSeekMode_;
use vst3::Steinberg::{
    int32, int64, kInvalidArgument, kPlatformTypeHWND, kResultFalse, kResultOk, kResultTrue,
    tresult, uint32, FIDString, FUnknown, IBStream, IBStreamTrait, IPluginBaseTrait, IPluginFactory,
    IPluginFactoryTrait, IPlugFrame, IPlugFrameTrait, IPlugView, IPlugViewTrait, PClassInfo,
    ViewRect, TUID,
};
use vst3::{Class, ComPtr, ComRef, ComWrapper};

use windows::core::{s, PCWSTR};
use windows::Win32::Foundation::{FreeLibrary, HMODULE, HWND};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

// ── host-implemented COM objects (the callbacks the plugin holds) ──────────────────────────

/// Minimal `IHostApplication` (mandatory — many plugins refuse `initialize` without one). Passed
/// as the `context` FUnknown to `IComponent::initialize`. Kept alive for the plugin's lifetime.
struct LfHostApp;
impl Class for LfHostApp {
    type Interfaces = (IHostApplication,);
}
impl IHostApplicationTrait for LfHostApp {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        // SAFETY: `name` is the plugin-provided String128 ([TChar;128]) buffer.
        let dst = &mut *name;
        let mut i = 0usize;
        for u in "BleepLoop".encode_utf16() {
            if i + 1 >= dst.len() {
                break;
            }
            dst[i] = u as _;
            i += 1;
        }
        dst[i] = 0;
        kResultOk
    }
    unsafe fn createInstance(
        &self,
        _cid: *mut TUID,
        iid: *mut TUID,
        obj: *mut *mut c_void,
    ) -> tresult {
        // Vend an IMessage when asked (P10.2). JUCE's separated edit-controller IPC: the
        // component hands the controller the in-process AudioProcessor pointer over the
        // connection via a host-created IMessage — without this, a separated controller's
        // createView returns null. Ownership transfers to the caller (refcount 1: new → +1
        // via to_com_ptr → into_raw moves that ref out → the local wrapper drop nets it to 1),
        // which releases it after `notify`. Other classes: not provided.
        if !iid.is_null() && !obj.is_null() && *iid == IMessage_iid {
            let wrapper = ComWrapper::new(LfMessage::new());
            if let Some(cptr) = wrapper.to_com_ptr::<IMessage>() {
                *obj = cptr.into_raw() as *mut c_void;
                return kResultOk;
            }
        }
        kResultFalse
    }
}

/// One value in an `LfAttributeList` (P10.2). JUCE stores its in-process pointers as `Int`; the
/// others round-trip faithfully so any host-message protocol the plugin uses keeps working.
enum AttrValue {
    Int(int64),
    Float(f64),
    Str(Vec<TChar>),
    Bin(Vec<u8>),
}

/// Copy a NUL-terminated `AttrID` (ASCII C-string) into an owned map key (empty for null).
unsafe fn attr_key(id: AttrID) -> Vec<u8> {
    if id.is_null() {
        Vec::new()
    } else {
        CStr::from_ptr(id).to_bytes().to_vec()
    }
}

/// Host `IAttributeList` (P10.2) — the key/value bag inside an `IMessage`. A faithful
/// single-threaded store: whatever the plugin `set`s under a key it reads back via `get` (JUCE
/// uses this to pass its edit-controller the AudioProcessor pointer). Owner-thread-only (the
/// connection `notify` runs there). `get` returns kResultFalse for a missing/mistyped key.
struct LfAttributeList {
    map: RefCell<HashMap<Vec<u8>, AttrValue>>,
}
impl LfAttributeList {
    fn new() -> Self {
        Self {
            map: RefCell::new(HashMap::new()),
        }
    }
}
impl Class for LfAttributeList {
    type Interfaces = (IAttributeList,);
}
impl IAttributeListTrait for LfAttributeList {
    unsafe fn setInt(&self, id: AttrID, value: int64) -> tresult {
        self.map
            .borrow_mut()
            .insert(attr_key(id), AttrValue::Int(value));
        kResultOk
    }
    unsafe fn getInt(&self, id: AttrID, value: *mut int64) -> tresult {
        if let Some(AttrValue::Int(v)) = self.map.borrow().get(&attr_key(id)) {
            if !value.is_null() {
                *value = *v;
            }
            kResultOk
        } else {
            kResultFalse
        }
    }
    unsafe fn setFloat(&self, id: AttrID, value: f64) -> tresult {
        self.map
            .borrow_mut()
            .insert(attr_key(id), AttrValue::Float(value));
        kResultOk
    }
    unsafe fn getFloat(&self, id: AttrID, value: *mut f64) -> tresult {
        if let Some(AttrValue::Float(v)) = self.map.borrow().get(&attr_key(id)) {
            if !value.is_null() {
                *value = *v;
            }
            kResultOk
        } else {
            kResultFalse
        }
    }
    unsafe fn setString(&self, id: AttrID, string: *const TChar) -> tresult {
        let mut v: Vec<TChar> = Vec::new();
        if !string.is_null() {
            let mut p = string;
            while *p != 0 {
                v.push(*p);
                p = p.add(1);
            }
        }
        v.push(0); // keep a terminator
        self.map
            .borrow_mut()
            .insert(attr_key(id), AttrValue::Str(v));
        kResultOk
    }
    unsafe fn getString(
        &self,
        id: AttrID,
        string: *mut TChar,
        size_in_bytes: uint32,
    ) -> tresult {
        let map = self.map.borrow();
        if let Some(AttrValue::Str(v)) = map.get(&attr_key(id)) {
            if string.is_null() {
                return kResultFalse;
            }
            let cap = (size_in_bytes as usize) / std::mem::size_of::<TChar>();
            if cap == 0 {
                return kResultFalse;
            }
            let n = v.len().min(cap); // v includes its NUL terminator → n >= 1
            std::ptr::copy_nonoverlapping(v.as_ptr(), string, n);
            *string.add(n - 1) = 0; // guarantee termination within the buffer
            kResultOk
        } else {
            kResultFalse
        }
    }
    unsafe fn setBinary(
        &self,
        id: AttrID,
        data: *const c_void,
        size_in_bytes: uint32,
    ) -> tresult {
        let mut v = vec![0u8; size_in_bytes as usize];
        if !data.is_null() && size_in_bytes > 0 {
            std::ptr::copy_nonoverlapping(
                data as *const u8,
                v.as_mut_ptr(),
                size_in_bytes as usize,
            );
        }
        self.map
            .borrow_mut()
            .insert(attr_key(id), AttrValue::Bin(v));
        kResultOk
    }
    unsafe fn getBinary(
        &self,
        id: AttrID,
        data: *mut *const c_void,
        size_in_bytes: *mut uint32,
    ) -> tresult {
        let map = self.map.borrow();
        if let Some(AttrValue::Bin(v)) = map.get(&attr_key(id)) {
            // Borrowed pointer into the stored Vec (valid until the key is overwritten or the
            // list drops); JUCE reads it immediately. Standard host behaviour.
            if !data.is_null() {
                *data = v.as_ptr() as *const c_void;
            }
            if !size_in_bytes.is_null() {
                *size_in_bytes = v.len() as uint32;
            }
            kResultOk
        } else {
            kResultFalse
        }
    }
}

/// Host `IMessage` (P10.2) — vended by `LfHostApp::createInstance(IMessage)` so the plugin can
/// send in-process notifications over its connection points (JUCE shares its AudioProcessor with
/// the separated edit-controller this way). Owns its attribute bag; `getAttributes` returns a
/// borrowed (non-owning) pointer valid for the message's lifetime. Owner-thread-only.
struct LfMessage {
    id: RefCell<CString>,
    attrs: ComWrapper<LfAttributeList>,
}
impl LfMessage {
    fn new() -> Self {
        Self {
            id: RefCell::new(CString::default()),
            attrs: ComWrapper::new(LfAttributeList::new()),
        }
    }
}
impl Class for LfMessage {
    type Interfaces = (IMessage,);
}
impl IMessageTrait for LfMessage {
    unsafe fn getMessageID(&self) -> FIDString {
        // Borrowed pointer into the owned CString — valid until setMessageID overwrites it or
        // the message drops; the receiver reads it immediately.
        self.id.borrow().as_ptr()
    }
    unsafe fn setMessageID(&self, id: FIDString) {
        *self.id.borrow_mut() = if id.is_null() {
            CString::default()
        } else {
            CStr::from_ptr(id).to_owned()
        };
    }
    unsafe fn getAttributes(&self) -> *mut IAttributeList {
        // Non-owning pointer into our owned attribute bag (alive for the message's lifetime).
        match self.attrs.as_com_ref::<IAttributeList>() {
            Some(r) => r.as_ptr(),
            None => std::ptr::null_mut(),
        }
    }
}

/// Host `IPlugFrame` — the resize callback the plugin's editor view holds (set via
/// `IPlugView::setFrame`). Owner-thread-only (built/stored/dropped inside the editor open/close
/// on the owner thread; the plugin holds the raw `.as_ptr()`, so the owning `ComWrapper` must
/// outlive the attached view — kept in `Vst3Editor::Open`). `hwnd` is the host window the view is
/// attached to, 0 until `vst3_editor_open` has created it (the frame is handed to the plugin
/// BEFORE the window exists, step 4 vs 6) — a resize that arrives in that gap is refused.
struct LfPlugFrame {
    hwnd: AtomicIsize,
}
impl LfPlugFrame {
    fn new() -> Self {
        Self {
            hwnd: AtomicIsize::new(0),
        }
    }
}
impl Class for LfPlugFrame {
    type Interfaces = (IPlugFrame,);
}
impl IPlugFrameTrait for LfPlugFrame {
    /// The plugin's editor wants a new size (its own size menu, a zoom step, a preset with another
    /// layout). The VST3 contract: the host resizes the parent's client area, then calls
    /// `IPlugView::onSize` with the size it granted — the view lays out against THAT call, so
    /// answering `kResultOk` without resizing (the pre-2026-09-12 behaviour) left the editor laid
    /// out for a size the window never took (clipped, or floating in a too-large frame).
    /// `view` is a borrow (`ComRef`, never an owning `ComPtr` — refcount footgun); the plugin
    /// calls this from its UI thread, which for a hosted editor is the owner thread that pumps it.
    unsafe fn resizeView(&self, view: *mut IPlugView, new_size: *mut ViewRect) -> tresult {
        let hwnd = self.hwnd.load(Acquire);
        if hwnd == 0 || view.is_null() || new_size.is_null() {
            return kResultFalse;
        }
        let rect = *new_size;
        let w = (rect.right - rect.left).max(1) as u32;
        let h = (rect.bottom - rect.top).max(1) as u32;
        let Some((got_w, got_h)) = set_client_size(HWND(hwnd as *mut c_void), w, h) else {
            log::warn!("[plugin_host] vst3 resizeView to {w}x{h} failed — window left as it was");
            return kResultFalse;
        };
        if (got_w, got_h) != (w, h) {
            // Windows clamped the window to the screen: the view is told the size it really has.
            log::info!("[plugin_host] vst3 resizeView asked {w}x{h}, screen allows {got_w}x{got_h}");
        }
        let mut granted = ViewRect {
            left: 0,
            top: 0,
            right: got_w as i32,
            bottom: got_h as i32,
        };
        match ComRef::<IPlugView>::from_raw(view) {
            Some(v) => v.onSize(&mut granted),
            None => kResultFalse,
        }
    }
}

/// Host `IBStream` over memory: what a VST3 plugin writes its state into (`getState`) and reads it
/// back from (`setState`, `setComponentState`) for engine mode's tones (`host/tone.rs`). Read, write,
/// seek and tell over one growable buffer, as the SDK's `MemoryStream`: a read returns what is left
/// and moves the cursor only by what it read (a read past the end leaves it where a seek put it), a
/// write past the end grows the buffer (zero-filling a gap a seek left), an empty transfer touches
/// nothing (its buffer may be null), and nothing grows past `tone::MAX_STATE_BYTES`. Owner-thread
/// only: the plugin uses it inside the call the host made, and the host reads the bytes back after it
/// returns.
pub(super) struct MemStream {
    bytes: RefCell<Vec<u8>>,
    pos: Cell<usize>,
}
impl MemStream {
    /// A stream at position 0 over `bytes`: a `setState` reads it, a `getState` writes into an empty one.
    pub(super) fn reading(bytes: &[u8]) -> Self {
        Self {
            bytes: RefCell::new(bytes.to_vec()),
            pos: Cell::new(0),
        }
    }
    /// What the plugin wrote.
    pub(super) fn bytes(&self) -> Vec<u8> {
        self.bytes.borrow().clone()
    }
}
impl Class for MemStream {
    type Interfaces = (IBStream,);
}
impl IBStreamTrait for MemStream {
    unsafe fn read(&self, buffer: *mut c_void, num_bytes: int32, num_bytes_read: *mut int32) -> tresult {
        if num_bytes < 0 || (buffer.is_null() && num_bytes > 0) {
            return kInvalidArgument;
        }
        let bytes = self.bytes.borrow();
        let at = self.pos.get();
        let n = (num_bytes as usize).min(bytes.len().saturating_sub(at));
        if n > 0 {
            // SAFETY: the plugin's buffer holds `num_bytes` ≥ n bytes; the source range is in bounds.
            std::ptr::copy_nonoverlapping(bytes.as_ptr().add(at), buffer.cast::<u8>(), n);
            self.pos.set(at + n);
        }
        if !num_bytes_read.is_null() {
            *num_bytes_read = n as int32;
        }
        kResultOk
    }
    unsafe fn write(&self, buffer: *mut c_void, num_bytes: int32, num_bytes_written: *mut int32) -> tresult {
        if num_bytes < 0 || (buffer.is_null() && num_bytes > 0) {
            return kInvalidArgument;
        }
        let at = self.pos.get();
        let n = num_bytes as usize;
        if n > 0 {
            let Some(end) = at.checked_add(n).filter(|&end| end <= super::super::tone::MAX_STATE_BYTES) else {
                return kResultFalse;
            };
            let mut bytes = self.bytes.borrow_mut();
            if bytes.len() < end {
                bytes.resize(end, 0);
            }
            // SAFETY: the plugin's buffer holds `num_bytes` bytes; the destination range was sized above.
            std::ptr::copy_nonoverlapping(buffer.cast::<u8>().cast_const(), bytes.as_mut_ptr().add(at), n);
            self.pos.set(end);
        }
        if !num_bytes_written.is_null() {
            *num_bytes_written = n as int32;
        }
        kResultOk
    }
    unsafe fn seek(&self, pos: int64, mode: int32, result: *mut int64) -> tresult {
        let base = match mode {
            IStreamSeekMode_::kIBSeekSet => 0,
            IStreamSeekMode_::kIBSeekCur => self.pos.get() as int64,
            IStreamSeekMode_::kIBSeekEnd => self.bytes.borrow().len() as int64,
            _ => return kInvalidArgument,
        };
        let limit = super::super::tone::MAX_STATE_BYTES as int64;
        let Some(to) = base.checked_add(pos).filter(|to| (0..=limit).contains(to)) else {
            return kInvalidArgument;
        };
        self.pos.set(to as usize);
        if !result.is_null() {
            *result = to;
        }
        kResultOk
    }
    unsafe fn tell(&self, pos: *mut int64) -> tresult {
        if pos.is_null() {
            return kInvalidArgument;
        }
        *pos = self.pos.get() as int64;
        kResultOk
    }
}

/// The plugin's pending `restartComponent` reports. OR-ed across calls, drained by the owner loop
/// once per turn: the cycle flags into ONE restart cycle, everything else into the log
/// line and, for `RELIST`, the re-list emit. A flag store only: `raise` never runs foreign code,
/// logs, emits, allocates or locks, so a plugin that (against the spec) reports from its own
/// worker thread is still safe.
#[derive(Default)]
struct RestartFlags {
    /// Pending `CYCLE` flags.
    cycle: AtomicI32,
    /// Pending non-cycle flags (informational, `RELIST`).
    notify: AtomicI32,
}
impl RestartFlags {
    /// The flags that need a deactivate → activate cycle; every other flag is informational.
    const CYCLE: i32 =
        RestartFlags_::kReloadComponent | RestartFlags_::kIoChanged | RestartFlags_::kLatencyChanged;
    /// The flags after which the web UI must re-list parameters (values or titles changed).
    const RELIST: i32 = RestartFlags_::kParamValuesChanged | RestartFlags_::kParamTitlesChanged;

    fn raise(&self, flags: i32) {
        let cycle = flags & Self::CYCLE;
        if cycle != 0 {
            self.cycle.fetch_or(cycle, Release);
        }
        let notify = flags & !Self::CYCLE;
        if notify != 0 {
            self.notify.fetch_or(notify, Release);
        }
    }
    /// Owner turn: the cycle flags raised so far (0 = no cycle pending).
    fn take(&self) -> i32 {
        self.cycle.swap(0, Acquire)
    }
    /// Owner turn: the non-cycle flags raised so far (0 = nothing to report).
    fn take_notify(&self) -> i32 {
        self.notify.swap(0, Acquire)
    }
}

/// Host `IComponentHandler` — the plugin's edit-controller calls these as its GUI moves a knob.
/// P10.3: `performEdit` forwards `(id, value)` onto BOTH (a) the main→audio ring as a
/// `PluginEvent::Param` (drained next block into the real `RtParamChanges` → the processor's
/// sound changes — required because Surge is separated-component, so `setParamNormalized` on the
/// controller alone never reaches the processor), and (b) the web UI via `plugin:param-changed`.
/// Runs on the OWNER/UI-pump thread (NEVER the audio thread), so the `Mutex` lock + `emit` are off
/// the hot path. The id is the controller's OWN id → valid by construction (no hash-id segfault
/// risk; that only applies to host-originated `setParameter`). ONE handler per load: the owner
/// sets it on the controller at load and keeps its `ComWrapper` alive until the owner exits; an
/// editor session uses it too and never sets its own, so closing an editor has nothing to restore
/// and a plugin that keeps a raw pointer never holds a dropped one.
struct LfComponentHandler {
    event_tx: Arc<Mutex<Producer<PluginEvent>>>,
    /// Tells the web UI a knob moved (`plugin:param-changed`, `id`, `value`). A closure rather
    /// than the window, so the handler is testable without one.
    param_changed: Box<dyn Fn(u32, f64) + Send + Sync>,
    restart: Arc<RestartFlags>,
}
impl Class for LfComponentHandler {
    type Interfaces = (IComponentHandler,);
}
impl IComponentHandlerTrait for LfComponentHandler {
    unsafe fn beginEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }
    unsafe fn performEdit(&self, id: ParamID, value_normalized: ParamValue) -> tresult {
        // Owner thread: serialise with the command-thread producers through the same Mutex (the
        // SPSC Producer stays single-logical). A full ring (unreachable at human knob rates,
        // 1024-deep / 256-drained-per-block) drops last-wins — inaudible.
        if let Ok(mut prod) = self.event_tx.lock() {
            let _ = prod.push(PluginEvent::Param {
                id: id as u32,
                value: value_normalized,
            });
        }
        (self.param_changed)(id as u32, value_normalized);
        kResultOk
    }
    unsafe fn endEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }
    /// The plugin reports that something changed behind the host's back. Every flag is raised on
    /// the shared `RestartFlags` and nothing else happens here: the owner loop logs it on its next
    /// turn, re-lists params for `RELIST` (a preset loaded inside the plugin, a program change)
    /// and runs a restart cycle for a cycle flag (`kReloadComponent`, `kIoChanged`,
    /// `kLatencyChanged`). Usually called on the owner thread (from a controller call the host
    /// itself made, or the editor pump); a foreign-thread call is safe because only atomics are
    /// touched — no log, no emit (audit B5).
    unsafe fn restartComponent(&self, flags: int32) -> tresult {
        self.restart.raise(flags);
        kResultOk
    }
}

/// Audio-thread-owned event storage shared (Rc) between the unit (which fills it each block)
/// and the `RtEventList` COM object the plugin reads in `process()`. Interior-mutable (every
/// IEventList method is `&self`); pre-grown so fill is alloc-free. Never crosses threads.
struct EventListInner {
    events: UnsafeCell<Vec<Event>>,
    count: Cell<usize>,
}
impl EventListInner {
    fn new() -> Self {
        let mut v = Vec::with_capacity(MAX_EVENTS_PER_BLOCK);
        for _ in 0..MAX_EVENTS_PER_BLOCK {
            // SAFETY: Event is a #[repr(C)] POD (primitive fields + a POD union); a zeroed
            // placeholder is valid and is overwritten before use (`count` gates the live ones).
            v.push(unsafe { std::mem::zeroed::<Event>() });
        }
        Self {
            events: UnsafeCell::new(v),
            count: Cell::new(0),
        }
    }
    fn clear(&self) {
        self.count.set(0);
    }
    fn push(&self, ev: Event) {
        let n = self.count.get();
        if n < MAX_EVENTS_PER_BLOCK {
            // SAFETY: single-threaded RT access; n < len (pre-grown to MAX_EVENTS_PER_BLOCK).
            unsafe {
                let v = &mut *self.events.get();
                v[n] = ev;
            }
            self.count.set(n + 1);
        }
    }
}

/// Host `IEventList` the plugin pulls note events from during `process()`.
struct RtEventList {
    inner: Rc<EventListInner>,
}
impl Class for RtEventList {
    type Interfaces = (IEventList,);
}
impl IEventListTrait for RtEventList {
    unsafe fn getEventCount(&self) -> i32 {
        self.inner.count.get() as i32
    }
    unsafe fn getEvent(&self, index: i32, e: *mut Event) -> tresult {
        if index < 0 || index as usize >= self.inner.count.get() {
            return kInvalidArgument;
        }
        // SAFETY: index in bounds; bitwise copy out (Event is POD with no Drop). `e` is the
        // plugin-provided out-pointer.
        let v = &*self.inner.events.get();
        let src = &v[index as usize] as *const Event;
        std::ptr::copy_nonoverlapping(src, e, 1);
        kResultOk
    }
    unsafe fn addEvent(&self, _e: *mut Event) -> tresult {
        // The host does not accept plugin-emitted (output) events in P10.1.
        kResultFalse
    }
}

/// Max distinct params carried into one `process()` block (one `IParamValueQueue` each). A
/// single editor + a human dragging knobs touches ~1/block; 64 is a generous ceiling, and a new
/// id past it waits in the ring for a later block (`drain_params`). Bounds the pre-grown queue
/// pool so the RT path is alloc-free (invariant #5).
const MAX_PARAM_QUEUES: usize = 64;

/// Interior-mutable storage for ONE param's pending change, shared (`Rc`) between the unit
/// on the audio thread (which writes the Cells each block) and the `RtParamQueue` COM object the plugin
/// reads in `process()`. Mirrors `EventListInner`'s split (the Cells can NOT live inside the COM
/// object — `ComWrapper` owns its data in an `Arc`, unreachable as a separate handle). One point
/// per queue (`sampleOffset 0`) is enough for an instantaneous knob set. Never crosses threads.
struct ParamQueueInner {
    id: Cell<ParamID>,
    value: Cell<ParamValue>,
}
impl ParamQueueInner {
    fn new() -> Self {
        Self {
            id: Cell::new(0),
            value: Cell::new(0.0),
        }
    }
}

/// Host `IParamValueQueue` the plugin reads (one per changed param). A single point at
/// `sampleOffset 0`. `addPoint` is the plugin→host (output) direction → refuse it.
struct RtParamQueue {
    inner: Rc<ParamQueueInner>,
}
impl Class for RtParamQueue {
    type Interfaces = (IParamValueQueue,);
}
impl IParamValueQueueTrait for RtParamQueue {
    unsafe fn getParameterId(&self) -> ParamID {
        self.inner.id.get()
    }
    unsafe fn getPointCount(&self) -> int32 {
        1
    }
    unsafe fn getPoint(
        &self,
        index: int32,
        sample_offset: *mut int32,
        value: *mut ParamValue,
    ) -> tresult {
        if index != 0 {
            return kInvalidArgument;
        }
        // SAFETY: the plugin supplies valid out-pointers for the synchronous process() call.
        *sample_offset = 0;
        *value = self.inner.value.get();
        kResultOk
    }
    unsafe fn addPoint(
        &self,
        _sample_offset: int32,
        _value: ParamValue,
        _index: *mut int32,
    ) -> tresult {
        kResultFalse
    }
}

/// RT-local storage backing the real host `IParameterChanges`. Holds a PRE-BUILT pool of
/// `RtParamQueue` COM objects (built once before the loop → alloc-free per block): the owning
/// `ComWrapper`s + `ComPtr`s are kept alive here (a cached raw `as_ptr()` alone would dangle —
/// `as_ptr` does no refcounting), and the `Rc<ParamQueueInner>` clones let the unit mutate
/// each queue's id/value. `push_param` coalesces per id (VST3 wants ≤1 queue per id/block).
struct ParamChangesInner {
    inners: Vec<Rc<ParamQueueInner>>,
    queue_ptrs: Vec<*mut IParamValueQueue>,
    _wrappers: Vec<ComWrapper<RtParamQueue>>,
    _cptrs: Vec<ComPtr<IParamValueQueue>>,
    count: Cell<usize>,
}
impl ParamChangesInner {
    /// Build the whole queue pool up front (the only allocation; runs in the loop preamble,
    /// before the alloc guard arms). `None` if a queue's `to_com_ptr` fails.
    fn new(pool: usize) -> Option<Self> {
        let mut inners = Vec::with_capacity(pool);
        let mut queue_ptrs = Vec::with_capacity(pool);
        let mut wrappers = Vec::with_capacity(pool);
        let mut cptrs = Vec::with_capacity(pool);
        for _ in 0..pool {
            let qinner = Rc::new(ParamQueueInner::new());
            let wrapper = ComWrapper::new(RtParamQueue {
                inner: qinner.clone(),
            });
            let cptr = wrapper.to_com_ptr::<IParamValueQueue>()?;
            queue_ptrs.push(cptr.as_ptr());
            inners.push(qinner);
            cptrs.push(cptr);
            wrappers.push(wrapper);
        }
        Some(Self {
            inners,
            queue_ptrs,
            _wrappers: wrappers,
            _cptrs: cptrs,
            count: Cell::new(0),
        })
    }
    fn clear(&self) {
        self.count.set(0);
    }
    /// Queue one param change (alloc-free). Coalesces per id (last value wins) so the plugin
    /// never sees two queues for the same id in a block. False, and nothing queued, when the id
    /// is new and the pool is full: the caller keeps the change for the next block.
    fn push_param(&self, id: ParamID, value: ParamValue) -> bool {
        let n = self.count.get();
        for q in &self.inners[..n] {
            if q.id.get() == id {
                q.value.set(value);
                return true;
            }
        }
        if n == self.inners.len() {
            return false;
        }
        self.inners[n].id.set(id);
        self.inners[n].value.set(value);
        self.count.set(n + 1);
        true
    }
}

/// RT: move up to `cap` events from the slot's ring into this block's `changes`, in ring order.
/// Nothing is dropped: a param whose id is new once the pool is full stays at the ring's head, and
/// the drain stops there, so it and every change behind it go out from the next block on, in order (a
/// change ahead of it to an id already queued coalesces; one behind it waits, even to such an id,
/// or it would overtake the waiting one). Notes do not ride this ring (`process` takes them from
/// its `events`); any that did are popped and ignored, as before.
fn drain_params(ring: &mut Consumer<PluginEvent>, changes: &ParamChangesInner, cap: usize) {
    for _ in 0..cap {
        let Ok(&event) = ring.peek() else { break };
        if let PluginEvent::Param { id, value } = event {
            if !changes.push_param(id as ParamID, value) {
                break;
            }
        }
        let _ = ring.pop();
    }
}

/// Host `IParameterChanges` the plugin reads each block (`getParameterCount` then
/// `getParameterData(i)`). Vends the pre-built pooled queues by cached raw ptr — no construction,
/// no refcounting, no alloc on the RT thread. `addParameterData` is plugin→host (output) → null.
struct RtParamChanges {
    inner: Rc<ParamChangesInner>,
}
impl Class for RtParamChanges {
    type Interfaces = (IParameterChanges,);
}
impl IParameterChangesTrait for RtParamChanges {
    unsafe fn getParameterCount(&self) -> int32 {
        self.inner.count.get() as int32
    }
    unsafe fn getParameterData(&self, index: int32) -> *mut IParamValueQueue {
        if index < 0 || index as usize >= self.inner.count.get() {
            return std::ptr::null_mut();
        }
        self.inner.queue_ptrs[index as usize]
    }
    unsafe fn addParameterData(
        &self,
        _id: *const ParamID,
        _index: *mut int32,
    ) -> *mut IParamValueQueue {
        std::ptr::null_mut()
    }
}

/// Translate a `PluginEvent` (from the main→audio ring) into a VST3 `Event`. Params ride
/// `IParameterChanges` (P10.3), not the event list, so `Param` is dropped here.
fn plugin_event_to_vst3(ev: PluginEvent) -> Option<Event> {
    // SAFETY: a zeroed Event is a valid POD base we then fill.
    let mut e: Event = unsafe { std::mem::zeroed() };
    e.busIndex = 0;
    e.sampleOffset = 0;
    e.ppqPosition = 0.0;
    e.flags = 0;
    match ev {
        PluginEvent::NoteOn { key, velocity } => {
            e.r#type = vst3::Steinberg::Vst::Event_::EventTypes_::kNoteOnEvent as u16;
            e.__field0 = Event__type0 {
                noteOn: NoteOnEvent {
                    channel: 0,
                    pitch: key as i16,
                    tuning: 0.0,
                    velocity: velocity as f32,
                    length: 0,
                    noteId: -1,
                },
            };
            Some(e)
        }
        PluginEvent::NoteOff { key } => {
            e.r#type = vst3::Steinberg::Vst::Event_::EventTypes_::kNoteOffEvent as u16;
            e.__field0 = Event__type0 {
                noteOff: NoteOffEvent {
                    channel: 0,
                    pitch: key as i16,
                    velocity: 0.0,
                    noteId: -1,
                    tuning: 0.0,
                },
            };
            Some(e)
        }
        PluginEvent::Param { .. } => None,
    }
}

type Vst3ModuleEntry = unsafe extern "system" fn() -> bool;

/// Owns one loaded VST3 DLL. Successful `InitDll` is paired with `ExitDll`, and the module stays
/// mapped until every vtbl-bearing COM object has been released by `teardown`.
struct Vst3Module {
    handle: HMODULE,
    exit_dll: Option<Vst3ModuleEntry>,
    init_succeeded: bool,
}

impl Vst3Module {
    fn load(path: PCWSTR) -> Result<Self, String> {
        // SAFETY: loading the vetted plugin DLL and resolving its standard VST3 module entries.
        // The guard owns the returned module reference immediately, so every later error unloads.
        let handle = unsafe { LoadLibraryW(path) }.map_err(|e| format!("LoadLibraryW: {e}"))?;
        let exit_dll = unsafe { GetProcAddress(handle, s!("ExitDll")) }
            .map(|p| unsafe { std::mem::transmute::<_, Vst3ModuleEntry>(p) });
        let mut module = Self {
            handle,
            exit_dll,
            init_succeeded: false,
        };
        if let Some(p) = unsafe { GetProcAddress(handle, s!("InitDll")) } {
            let init = unsafe { std::mem::transmute::<_, Vst3ModuleEntry>(p) };
            if !unsafe { init() } {
                return Err("InitDll returned false".to_string());
            }
            module.init_succeeded = true;
        }
        Ok(module)
    }

    fn handle(&self) -> HMODULE {
        self.handle
    }
}

impl Drop for Vst3Module {
    fn drop(&mut self) {
        // SAFETY: ExitDll is called only when this guard's matching InitDll succeeded. `teardown`
        // explicitly releases every module COM object before dropping the guard on successful loads.
        unsafe {
            if self.init_succeeded {
                if let Some(exit_dll) = self.exit_dll {
                    let _ = exit_dll();
                }
            }
            let _ = FreeLibrary(self.handle);
        }
    }
}

/// Owner-local VST3 editor state. `Open` carries everything that must outlive the attached
/// view: the `IPlugView` (released LAST, after `removed()`), our `IPlugFrame` `ComWrapper` (the
/// plugin holds a raw ptr to it, so it must not drop while the editor is open), and the host
/// window (RAII `DestroyWindow` on drop). Field order is load-bearing: drop runs view → frame →
/// win, and `removed()` is called first. The component handler is the load's, not the editor's.
enum Vst3Editor {
    Closed,
    Open {
        view: ComPtr<IPlugView>,
        _frame: ComWrapper<LfPlugFrame>,
        win: HostWindow,
    },
}

/// Open the VST3 editor on the owner thread. `host_hwnd` (0 = unknown) is the main window, used
/// as the host editor window's owner. Requires a single-component controller (Surge); None →
/// clear Err (separated-component editor is deferred to P10.3). Mirrors the CLAP `editor_open`
/// embedded path: createView → isPlatformTypeSupported(HWND) → setFrame → getSize → host window →
/// attached → onSize → show. The controller keeps the load-time component handler
/// (`LfComponentHandler`); an editor never replaces it. `kResultTrue == 0`, so compare with `==`.
fn vst3_editor_open(
    controller: &Option<ComPtr<IEditController>>,
    host_hwnd: usize,
    slot: u8,
) -> Result<Vst3Editor, String> {
    let ctl = controller.as_ref().ok_or_else(|| {
        "VST3 plugin exposes no IEditController (separated-component → P10.3)".to_string()
    })?;
    // SAFETY: every call runs on the owner thread; `ctl` is a live IEditController for the
    // loaded plugin, and all raw pointers handed across the FFI are valid for their call.
    unsafe {
        // 1. No component handler here: the controller keeps the load's (`LfComponentHandler`).
        // 2. Create the view. createView returns a RAW *mut IPlugView, already add_ref'd → own it
        //    once with from_raw (no extra add_ref). Null = the plugin has no editor.
        log::info!("[plugin_host] vst3 editor_open slot {slot}: createView…");
        let raw = ctl.createView(ViewType::kEditor);
        let view = ComPtr::<IPlugView>::from_raw(raw)
            .ok_or_else(|| "createView returned null (plugin has no editor)".to_string())?;
        log::info!("[plugin_host] vst3 editor_open slot {slot}: createView ok");
        // 3. Must support HWND embedding. kResultTrue == kResultOk == 0 on Windows.
        if view.isPlatformTypeSupported(kPlatformTypeHWND) != kResultTrue {
            return Err("VST3 view does not support HWND embedding".to_string());
        }
        // 4. Give the plugin our IPlugFrame BEFORE attach (some plugins query it during attach).
        let frame = ComWrapper::new(LfPlugFrame::new());
        if let Some(fp) = frame.to_com_ptr::<IPlugFrame>() {
            let _ = view.setFrame(fp.as_ptr());
        }
        // 5. Ask the plugin its preferred size (ViewRect: w = right-left, h = bottom-top).
        let mut rect: ViewRect = std::mem::zeroed();
        let (w, h) = if view.getSize(&mut rect) == kResultOk {
            (
                (rect.right - rect.left).max(1) as u32,
                (rect.bottom - rect.top).max(1) as u32,
            )
        } else {
            (900, 600) // mirror the CLAP editor_open fallback
        };
        // 6. Host window (reused rt_host helper; sizes the CLIENT area via AdjustWindowRectEx).
        let owner = if host_hwnd != 0 {
            Some(HWND(host_hwnd as *mut c_void))
        } else {
            None
        };
        let win = create_host_window(w, h, owner)?;
        // From here a plugin-initiated `resizeView` has a window to resize.
        frame.hwnd.store(win.hwnd.0 as isize, Release);
        // 7. Attach the plugin view into our window. windows-crate HWND.0 is already *mut c_void.
        log::info!("[plugin_host] vst3 editor_open slot {slot}: attached…");
        if view.attached(win.hwnd.0 as *mut c_void, kPlatformTypeHWND) != kResultOk {
            return Err("IPlugView::attached failed".to_string());
        }
        // 8. Tell the plugin the final size, then show our window. The size is read back from the
        //    window, not reused from step 5: a plugin that calls `resizeView` from inside
        //    `attached()` (its saved zoom/size) has already resized the client area, and repeating
        //    the pre-attach size here would undo that layout.
        let (w, h) = client_size(win.hwnd);
        let mut final_rect = ViewRect {
            left: 0,
            top: 0,
            right: w as i32,
            bottom: h as i32,
        };
        let _ = view.onSize(&mut final_rect);
        show_host_window_front(win.hwnd);
        log::info!("[plugin_host] VST3 editor embedded into host window ({w}x{h})");
        Ok(Vst3Editor::Open {
            view,
            _frame: frame,
            win,
        })
    }
}

/// Close the VST3 editor: `removed()` detaches the plugin's child from our window FIRST (while
/// the view is still live), THEN the drop runs view-release → frame-release →
/// `HostWindow::drop` (DestroyWindow). Mirrors the CLAP `gui.destroy` → DestroyWindow order.
fn vst3_editor_close(ed: &mut Vst3Editor) {
    if let Vst3Editor::Open { view, .. } = ed {
        // SAFETY: owner thread; detach the plugin's child before our window is destroyed.
        unsafe {
            let _ = view.removed();
        }
    }
    *ed = Vst3Editor::Closed; // drops view/frame then HostWindow::drop → DestroyWindow
    // Pump the WM_DESTROY/NCDESTROY + JUCE-posted teardown messages on THIS owner thread before
    // the loop falls back to its blocking recv_timeout (which stops pumping). See the helper doc.
    drain_after_editor_teardown();
}

/// Obtain the plugin's edit controller, handling both VST3 component models. Single-component
/// plugins expose `IEditController` on the `IComponent` itself (`.cast`); separated-component
/// plugins (JUCE/Surge VST3) host it as a distinct class — `getControllerClassId` → factory
/// `createInstance` → `initialize`. Returns `(controller, separated)`; `separated` drives the
/// extra `terminate()` at teardown. Any failure → `(None, false)` (the editor replies a clear
/// Err). P10.2 wires controller CREATION only; the connection-point relay + `setComponentState`
/// sync (needed for the editor's knobs to move the processor's sound) are P10.3.
unsafe fn obtain_controller(
    factory: &ComPtr<IPluginFactory>,
    component: &ComPtr<IComponent>,
    host_ctx: &ComPtr<FUnknown>,
) -> (Option<ComPtr<IEditController>>, bool) {
    // Single-component: the IComponent also implements IEditController (same object).
    if let Some(c) = component.cast::<IEditController>() {
        return (Some(c), false);
    }
    // Separated-component: create the distinct controller class the component names.
    let mut cid: TUID = std::mem::zeroed();
    if component.getControllerClassId(&mut cid) != kResultOk {
        return (None, false);
    }
    let mut obj: *mut c_void = std::ptr::null_mut();
    if factory.createInstance(cid.as_ptr(), IEditController_iid.as_ptr(), &mut obj) != kResultOk
        || obj.is_null()
    {
        return (None, false);
    }
    let controller = match ComPtr::<IEditController>::from_raw(obj as *mut IEditController) {
        Some(c) => c,
        None => return (None, false),
    };
    if controller.initialize(host_ctx.as_ptr()) != kResultOk {
        let _ = controller.terminate();
        return (None, false);
    }
    // Cross-connect the component's and controller's connection points so JUCE can hand the
    // controller the in-process AudioProcessor over a host-vended IMessage — required, or the
    // separated controller's createView returns null. Best-effort: skip if either lacks one.
    if let (Some(comp_cp), Some(ctrl_cp)) = (
        component.cast::<IConnectionPoint>(),
        controller.cast::<IConnectionPoint>(),
    ) {
        let _ = comp_cp.connect(ctrl_cp.as_ptr());
        let _ = ctrl_cp.connect(comp_cp.as_ptr());
    }
    (Some(controller), true)
}

/// Names of the `restartComponent` flags set in `flags`, for the log (`0x…` when none is known).
fn restart_flag_names(flags: int32) -> String {
    const NAMES: [(int32, &str); 12] = [
        (RestartFlags_::kReloadComponent, "reload"),
        (RestartFlags_::kIoChanged, "io"),
        (RestartFlags_::kParamValuesChanged, "param-values"),
        (RestartFlags_::kLatencyChanged, "latency"),
        (RestartFlags_::kParamTitlesChanged, "param-titles"),
        (RestartFlags_::kMidiCCAssignmentChanged, "midi-cc"),
        (RestartFlags_::kNoteExpressionChanged, "note-expression"),
        (RestartFlags_::kIoTitlesChanged, "io-titles"),
        (RestartFlags_::kPrefetchableSupportChanged, "prefetchable"),
        (RestartFlags_::kRoutingInfoChanged, "routing"),
        (RestartFlags_::kKeyswitchChanged, "keyswitch"),
        (RestartFlags_::kParamIDMappingChanged, "param-id-mapping"),
    ];
    let names: Vec<&str> = NAMES
        .iter()
        .filter(|(bit, _)| flags & bit != 0)
        .map(|(_, n)| *n)
        .collect();
    if names.is_empty() {
        format!("0x{flags:x}")
    } else {
        names.join("+")
    }
}

/// What one `activate_component` negotiated. The channel counts size the RT loop's per-channel
/// buffers and `ProcessData` (a wrong count is the Surge hash-param crash class), so every RT spawn
/// takes the activation it runs under. `latency_frames` is read for the log only: BleepLoop monitors
/// natively, so plugin latency cancels (`AGENTS.md` § Record-latency compensation).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Activation {
    out_channels: u32,
    in_channels: u32, // P11.2: 0 = no audio-input bus (synth) — keeps the numInputs=0 layout
    latency_frames: u32,
}

/// The one VST3 activation sequence, run at load and again after every plugin-requested restart:
/// request stereo buses, read back what the plugin actually gave, `setupProcessing` at D /
/// `max_frames`, activate the buses, `setActive(1)`, read the latency. Requires an initialised,
/// INACTIVE component that nothing processes; a failure leaves it inactive.
///
/// # Safety
/// Owner thread only; `component` and `processor` are live handles to the same plugin object.
unsafe fn activate_component(
    component: &ComPtr<IComponent>,
    processor: &ComPtr<IAudioProcessor>,
    device_rate: f64,
    max_frames: u32,
) -> Result<Activation, String> {
    // Bus config (instrument: 0 audio inputs, 1 audio output + event input for notes).
    let k_audio = MediaTypes_::kAudio as i32;
    let k_event = MediaTypes_::kEvent as i32;
    let k_out = BusDirections_::kOutput as i32;
    let k_in = BusDirections_::kInput as i32;

    let out_channels = checked_plugin_channels(
        {
            let mut bi: BusInfo = std::mem::zeroed();
            if component.getBusInfo(k_audio, k_out, 0, &mut bi) == kResultOk {
                bi.channelCount as i64
            } else {
                2
            }
        },
        "VST3 output bus 0",
        false,
    )?;
    // P11.2: does the plugin have an audio INPUT bus? (A synth/instrument has 0 → the whole
    // input path stays exactly the P10 zero-input layout.)
    let in_bus_count = component.getBusCount(k_audio, k_in);
    // Request stereo-in (when present) + stereo-out. The plugin may renegotiate; we size buffers
    // to the QUERIED count below, never the requested arrangement.
    let mut ins = [SpeakerArr::kStereo];
    let mut outs = [SpeakerArr::kStereo];
    let _ = processor.setBusArrangements(
        if in_bus_count > 0 { ins.as_mut_ptr() } else { std::ptr::null_mut() },
        if in_bus_count > 0 { 1 } else { 0 },
        outs.as_mut_ptr(),
        1,
    );
    // Query the ACTUAL input channel count AFTER setBusArrangements — do NOT trust the requested
    // arrangement (Neural DSP may give mono-in/stereo-out; sizing the buffers to a wrong count
    // reads garbage — the Surge hash-param crash class).
    let in_channels = if in_bus_count > 0 {
        let mut bi: BusInfo = std::mem::zeroed();
        if component.getBusInfo(k_audio, k_in, 0, &mut bi) == kResultOk {
            checked_plugin_channels(bi.channelCount as i64, "VST3 input bus 0", true)?
        } else {
            0
        }
    } else {
        0
    };
    // Re-query the ACTUAL output channel count AFTER setBusArrangements too (mirror the input
    // side): the negotiated output arrangement can differ from the pre-arrangement getBusInfo.
    // out_channels drives the per-block buffer alloc, ProcessData numChannels, and the
    // sum_to_mono divisor — sizing them to a stale count desyncs the wet mono gain (or, if the
    // negotiated count grew, reads past the buffer). No-op for a stereo plugin whose default
    // already equals the negotiated output (Surge/Petrucci). (Bug-hunt 2026-06-21, #7.)
    let out_channels = {
        let mut bi: BusInfo = std::mem::zeroed();
        if component.getBusInfo(k_audio, k_out, 0, &mut bi) == kResultOk {
            checked_plugin_channels(bi.channelCount as i64, "VST3 output bus 0", false)?
        } else {
            out_channels // fall back to the pre-arrangement query
        }
    };

    let mut proc_setup = ProcessSetup {
        processMode: ProcessModes_::kRealtime as i32,
        symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
        maxSamplesPerBlock: max_frames as i32,
        sampleRate: device_rate,
    };
    if processor.setupProcessing(&mut proc_setup) != kResultOk {
        return Err("setupProcessing failed".to_string());
    }

    let _ = component.activateBus(k_audio, k_out, 0, 1);
    // P11.2: activate the audio input bus too — guarded on the QUERIED channel count (not just
    // bus presence) so activation agrees with what the RT loop feeds; a 0-channel input bus is
    // treated as a synth (numInputs=0).
    if in_channels > 0 {
        let _ = component.activateBus(k_audio, k_in, 0, 1);
    }
    if component.getBusCount(k_event, k_in) > 0 {
        let _ = component.activateBus(k_event, k_in, 0, 1);
    }
    if component.setActive(1) != kResultOk {
        return Err("setActive(true) failed".to_string());
    }
    // Valid once active. Logged only — see `Activation`.
    let latency_frames = processor.getLatencySamples();
    Ok(Activation {
        out_channels,
        in_channels,
        latency_frames,
    })
}

/// Deactivate + terminate the component, release every module COM object in order, then
/// drop the module LAST (after all vtbl-bearing objects are dropped, so we never unmap live code).
/// Returns the ms spent in `setActive(0)`, `terminate`, the COM releases and the module drop.
/// ASSUMES the plugin tears down its own threads/timers in `terminate()` (Surge does). A
/// plugin that leaves a worker thread or OS timer running past `terminate` could fire into
/// unmapped code after FreeLibrary — if a future plugin proves flaky here, skip/defer the
/// FreeLibrary (many hosts never unload) rather than risk the crash. (P10.1 review 🟡-3.)
fn teardown(
    component: ComPtr<IComponent>,
    host_ctx: ComPtr<FUnknown>,
    hostapp: ComWrapper<LfHostApp>,
    factory: ComPtr<IPluginFactory>,
    module: Vst3Module,
) -> [u128; 4] {
    // SAFETY: the caller has stopped processing; deactivate + terminate on the owner thread.
    let t = Instant::now();
    unsafe {
        let _ = component.setActive(0);
    }
    let deactivate_ms = t.elapsed().as_millis();
    let t = Instant::now();
    unsafe {
        let _ = component.terminate();
    }
    let terminate_ms = t.elapsed().as_millis();
    let t = Instant::now();
    drop(component);
    drop(host_ctx);
    drop(hostapp);
    drop(factory);
    let release_ms = t.elapsed().as_millis();
    let t = Instant::now();
    drop(module);
    [deactivate_ms, terminate_ms, release_ms, t.elapsed().as_millis()]
}

/// Decode a VST3 `String128` (`[i16; 128]` UTF-16, NUL-terminated) into a Rust `String`.
fn string128_to_string(s: &String128) -> String {
    let units: Vec<u16> = s.iter().take_while(|&&c| c != 0).map(|&c| c as u16).collect();
    String::from_utf16_lossy(&units)
}

/// Enumerate the VST3 plugin's automatable params off the edit controller (owner-thread call —
/// the controller is `!Send`). Skips hidden params. VST3 param values are always normalised
/// 0..1, so `min/max` are 0.0/1.0 and `default` is `defaultNormalizedValue`. `None` controller
/// (plugin has no editor controller) → empty list, so the UI degrades gracefully.
fn list_vst3_params(
    controller: &Option<ComPtr<IEditController>>,
) -> Result<Vec<ParamDesc>, String> {
    let ctl = match controller {
        Some(c) => c,
        None => return Ok(Vec::new()),
    };
    // SAFETY: owner thread; `ctl` is a live IEditController for the loaded plugin.
    unsafe {
        let count = ctl.getParameterCount();
        let mut out =
            Vec::with_capacity(checked_plugin_params(count as i64, "VST3 edit controller")?);
        for i in 0..count {
            let mut info: ParameterInfo = std::mem::zeroed();
            if ctl.getParameterInfo(i, &mut info) != kResultOk {
                continue;
            }
            if info.flags
                & vst3::Steinberg::Vst::ParameterInfo_::ParameterFlags_::kIsHidden
                != 0
            {
                continue;
            }
            out.push(ParamDesc {
                id: info.id,
                name: string128_to_string(&info.title),
                min_value: 0.0,
                max_value: 1.0,
                default_value: info.defaultNormalizedValue,
                value: ctl.getParamNormalized(info.id), // live, not default
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
#[path = "vst3_restart_fixture.rs"]
mod restart_tests;
#[cfg(test)]
#[path = "vst3_resize_fixture.rs"]
mod resize_tests;
#[cfg(test)]
#[path = "vst3_controller_fixture.rs"]
mod controller_tests;

// Engine mode: this format's unit and owner (`engine_slot.rs`).
#[allow(dead_code)]
#[path = "vst3_engine.rs"]
pub(super) mod engine;

// DEV Stage 1 premise spike (`docs/ARCHITECTURE.md` § Measured premise): the VST3 load + process sequence,
// copied into one native device callback. A child here to reach the private load items.
#[cfg(debug_assertions)]
#[path = "engine_spike.rs"]
mod engine_spike;
#[cfg(debug_assertions)]
pub(crate) use engine_spike::run as engine_spike_run;

/// Audit B5: `restartComponent`'s whole body is `RestartFlags::raise`, so a plugin calling it from
/// its own worker thread must touch nothing but atomics there — no log (a lock + allocation), no
/// emit and no event-ring push. The owner's drain then sees every flag, split by kind.
#[cfg(test)]
mod restart_flag_tests {
    use super::*;
    use rtrb::RingBuffer;
    use std::sync::atomic::AtomicUsize;
    use std::thread::ThreadId;

    /// Counts log records emitted on one watched thread; every other thread's records pass by.
    struct ThreadLogCounter {
        watched: Mutex<Option<ThreadId>>,
        count: AtomicUsize,
    }
    impl log::Log for ThreadLogCounter {
        fn enabled(&self, _: &log::Metadata) -> bool {
            true
        }
        fn log(&self, _: &log::Record) {
            let me = std::thread::current().id();
            if *self.watched.lock().unwrap() == Some(me) {
                self.count.fetch_add(1, Relaxed);
            }
        }
        fn flush(&self) {}
    }
    static COUNTER: ThreadLogCounter = ThreadLogCounter {
        watched: Mutex::new(None),
        count: AtomicUsize::new(0),
    };

    /// Run `f` on a fresh thread and return how many log records it emitted there.
    fn logs_on_a_foreign_thread(f: impl FnOnce() + Send + 'static) -> usize {
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let t = std::thread::spawn(move || {
            go_rx.recv().unwrap();
            f();
        });
        *COUNTER.watched.lock().unwrap() = Some(t.thread().id());
        let before = COUNTER.count.load(Relaxed);
        go_tx.send(()).unwrap();
        t.join().unwrap();
        *COUNTER.watched.lock().unwrap() = None;
        COUNTER.count.load(Relaxed) - before
    }

    #[test]
    fn restart_component_from_a_foreign_thread_only_raises_flags() {
        let _ = log::set_logger(&COUNTER);
        log::set_max_level(log::LevelFilter::Trace);
        // The counter works: a thread that logs is seen.
        assert_eq!(
            logs_on_a_foreign_thread(|| log::info!("control record")),
            1,
            "the counting logger must be installed for this test to mean anything"
        );

        // The production handler, reached the way a plugin reaches it: through its vtable.
        let restart = Arc::new(RestartFlags::default());
        let (event_tx, mut event_rx) = RingBuffer::<PluginEvent>::new(4);
        let emitted = Arc::new(AtomicUsize::new(0));
        let emits = emitted.clone();
        let handler = ComWrapper::new(LfComponentHandler {
            event_tx: Arc::new(Mutex::new(event_tx)),
            param_changed: Box::new(move |_, _| {
                emits.fetch_add(1, Relaxed);
            }),
            restart: restart.clone(),
        });
        let hp = handler.to_com_ptr::<IComponentHandler>().expect("IComponentHandler");
        // The sink and the ring are live: an edit reaches both (so "none" below means something).
        // SAFETY: `hp` is a live pointer to `handler`, held for every call in this test.
        assert_eq!(unsafe { hp.performEdit(7, 0.5) }, kResultOk);
        assert_eq!(emitted.load(Relaxed), 1);
        assert!(event_rx.pop().is_ok());

        let plugin_thread_hp = hp.clone();
        let logged = logs_on_a_foreign_thread(move || {
            // SAFETY: as above; the clone keeps the handler alive on the plugin's thread.
            let r = unsafe {
                plugin_thread_hp.restartComponent(
                    RestartFlags_::kParamValuesChanged
                        | RestartFlags_::kLatencyChanged
                        | RestartFlags_::kMidiCCAssignmentChanged,
                )
            };
            assert_eq!(r, kResultOk);
        });
        assert_eq!(logged, 0, "restartComponent must not log on the plugin's thread");
        assert_eq!(emitted.load(Relaxed), 1, "restartComponent must not emit");
        assert!(event_rx.pop().is_err(), "restartComponent must not push to the event ring");

        // The owner turn: the cycle flag for the restart cycle, the rest for the log + re-list.
        assert_eq!(restart.take(), RestartFlags_::kLatencyChanged);
        let notices = restart.take_notify();
        assert_eq!(
            notices,
            RestartFlags_::kParamValuesChanged | RestartFlags_::kMidiCCAssignmentChanged
        );
        assert_ne!(notices & RestartFlags::RELIST, 0);
        assert_eq!((restart.take(), restart.take_notify()), (0, 0), "drained once");
    }
}

/// The param drain (`drain_params`): a change whose id would need a queue past the pool waits in
/// the ring for the next block, in order, and none is lost. Read back through the host
/// `IParameterChanges` the plugin is handed, as a plugin reads it.
#[cfg(test)]
mod param_drain_tests {
    use super::*;
    use rtrb::RingBuffer;

    /// One block's `IParameterChanges` as the plugin reads it: each queue's (id, value).
    fn block(ring: &mut Consumer<PluginEvent>, changes: &Rc<ParamChangesInner>) -> Vec<(ParamID, ParamValue)> {
        let com = ComWrapper::new(RtParamChanges { inner: changes.clone() });
        let list = com.to_com_ptr::<IParameterChanges>().expect("the changes list");
        changes.clear();
        drain_params(ring, changes, MAX_EVENTS_PER_BLOCK);
        // SAFETY: the list and its pooled queues live for the calls, as during `process`.
        unsafe {
            (0..list.getParameterCount())
                .map(|i| {
                    let queue = ComRef::from_raw(list.getParameterData(i)).expect("a queue");
                    assert_eq!(queue.getPointCount(), 1);
                    let (mut offset, mut value) = (-1, -1.0);
                    assert_eq!(queue.getPoint(0, &mut offset, &mut value), kResultOk);
                    assert_eq!(offset, 0);
                    (queue.getParameterId(), value)
                })
                .collect()
        }
    }

    fn setup(events: &[(u32, f64)]) -> (Consumer<PluginEvent>, Rc<ParamChangesInner>) {
        let (mut tx, rx) = RingBuffer::new(1024);
        for &(id, value) in events {
            tx.push(PluginEvent::Param { id, value }).unwrap();
        }
        (rx, Rc::new(ParamChangesInner::new(MAX_PARAM_QUEUES).expect("the queue pool")))
    }

    fn value(id: u32) -> f64 {
        id as f64 / 1000.0
    }

    #[test]
    fn a_new_id_past_the_pool_waits_for_the_next_block() {
        let ids = 0..MAX_PARAM_QUEUES as u32 + 1;
        let (mut ring, changes) = setup(&ids.clone().map(|id| (id, value(id))).collect::<Vec<_>>());
        let first = block(&mut ring, &changes);
        assert_eq!(first.len(), MAX_PARAM_QUEUES, "the first block carries a full pool");
        assert_eq!(first, ids.clone().take(MAX_PARAM_QUEUES).map(|id| (id, value(id))).collect::<Vec<_>>());
        let last = MAX_PARAM_QUEUES as u32;
        assert_eq!(block(&mut ring, &changes), vec![(last, value(last))], "the 65th, next block");
        assert!(ring.is_empty());
        assert!(block(&mut ring, &changes).is_empty(), "nothing left over");
    }

    /// With the pool full: a change to an id already queued, ahead of a new id, coalesces into this
    /// block (last value wins); the new id waits, and so does everything behind it, a change to a
    /// queued id included, so no change overtakes one pushed before it.
    #[test]
    fn a_change_ahead_of_the_waiting_id_coalesces_and_one_behind_it_waits() {
        let mut events: Vec<(u32, f64)> = (0..MAX_PARAM_QUEUES as u32).map(|id| (id, value(id))).collect();
        let new = MAX_PARAM_QUEUES as u32;
        events.extend([(5, 0.5), (new, value(new)), (5, 0.25), (7, 0.75)]);
        let (mut ring, changes) = setup(&events);
        let first = block(&mut ring, &changes);
        assert_eq!(first.len(), MAX_PARAM_QUEUES);
        assert_eq!(first[5], (5, 0.5), "coalesced: the change ahead of the waiting id");
        assert_eq!(first[7], (7, value(7)), "the change behind it waits");
        assert_eq!(block(&mut ring, &changes), vec![(new, value(new)), (5, 0.25), (7, 0.75)], "next block, in order");
        assert!(ring.is_empty());
    }
}
