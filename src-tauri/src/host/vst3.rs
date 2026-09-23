use super::{
    promote_pro_audio, publish_param_ids, revert, spawn_rt_then_ready, sum_to_mono,
    wasapi_period_frames, OwnerRequest, ParamIds, PluginEvent, RtJoinGuard, RtRings,
    MAX_EVENTS_PER_BLOCK, OUT_RING_CAP,
};
use super::super::editor_window::{
    client_size, create_host_window, drain_after_editor_teardown, pump_thread_messages,
    set_client_size, show_host_window_front, wait_for_input, HostWindow,
};
use super::super::native_io::NativeIo;
use super::super::transport::{
    checked_plugin_channels, create_shared_ring, force_device_rate, report_new_rt_faults, Hop1Pipe,
    InPipe, LoadReady, OutMonitorPipe, ProducerDiag, RtFault, SharedBufferHandle, HOP1_CAPACITY_FRAMES,
    TARGET_FILL_SECONDS,
};
use super::super::state::{ParamDesc, PluginDescriptor, PluginInfo};

use std::cell::{Cell, RefCell, UnsafeCell};
use std::collections::HashMap;
use std::ffi::{c_void, CStr, CString};
use std::os::windows::ffi::OsStrExt;
use std::rc::Rc;
use std::sync::atomic::{
    AtomicBool, AtomicI32, AtomicIsize, AtomicU32,
    Ordering::{Acquire, Relaxed, Release},
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rtrb::{Consumer, Producer};
use tauri::Emitter;

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
use vst3::Steinberg::{
    int32, int64, kInvalidArgument, kPlatformTypeHWND, kResultFalse, kResultOk, kResultTrue,
    tresult, uint32, FIDString, FUnknown, IPluginBaseTrait, IPluginFactory,
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

/// The `plugin:param-changed` emit payload (editor knob → web UI). Single-word fields → serde
/// camelCase is a no-op; JS reads `payload.slot/.id/.value`. `id` is the param's stable id
/// (same space as `setParameter`/`listParams`), `value` the new normalised 0..1 value.
#[derive(Clone, serde::Serialize)]
struct ParamChanged {
    slot: u8,
    id: u32,
    value: f64,
}

/// The plugin's pending `restartComponent` reports. OR-ed across calls and across BOTH handler
/// instances (the load-time one and an open editor's), drained by the owner loop once per turn:
/// the cycle flags into ONE `service_vst3_restart`, everything else into the log line and, for
/// `RELIST`, the re-list emit. A flag store only: `raise` never runs foreign code, logs, emits,
/// allocates or locks, so a plugin that (against the spec) reports from its own worker thread is
/// still safe.
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
/// Runs on the OWNER/UI-pump thread (NEVER the RT thread), so the `Mutex` lock + `emit` are off
/// the hot path. The id is the controller's OWN id → valid by construction (no hash-id segfault
/// risk; that only applies to host-originated `setParameter`). The owning `ComWrapper` is kept
/// alive in `Vst3Editor::Open` (the plugin holds a raw ptr to it for the editor session).
struct LfComponentHandler {
    event_tx: Arc<Mutex<Producer<PluginEvent>>>,
    slot: u8,
    window: tauri::WebviewWindow,
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
        let _ = self.window.emit(
            "plugin:param-changed",
            ParamChanged {
                slot: self.slot,
                id: id as u32,
                value: value_normalized,
            },
        );
        kResultOk
    }
    unsafe fn endEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }
    /// The plugin reports that something changed behind the host's back. Every flag is raised on
    /// the shared `RestartFlags` and nothing else happens here: the owner loop logs it on its next
    /// turn, re-lists params for `RELIST` (a preset loaded inside the plugin, a program change)
    /// and runs `service_vst3_restart` for a cycle flag (`kReloadComponent`, `kIoChanged`,
    /// `kLatencyChanged`). Usually called on the owner thread (from a controller call the host
    /// itself made, or the editor pump); a foreign-thread call is safe because only atomics are
    /// touched — no log, no emit (audit B5).
    unsafe fn restartComponent(&self, flags: int32) -> tresult {
        self.restart.raise(flags);
        kResultOk
    }
}

/// RT-thread-owned event storage shared (Rc) between the producer (which fills it each block)
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
/// single editor + a human dragging knobs touches ~1/block; 64 is a generous ceiling. Bounds
/// the pre-grown queue pool so the RT path is alloc-free (invariant #5).
const MAX_PARAM_QUEUES: usize = 64;

/// Interior-mutable storage for ONE param's pending change, shared (`Rc`) between the RT
/// producer (which writes the Cells each block) and the `RtParamQueue` COM object the plugin
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
/// `as_ptr` does no refcounting), and the `Rc<ParamQueueInner>` clones let the producer mutate
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
    /// never sees two queues for the same id in a block; overflow past the pool is dropped.
    fn push_param(&self, id: ParamID, value: ParamValue) {
        let n = self.count.get();
        for q in &self.inners[..n] {
            if q.id.get() == id {
                q.value.set(value);
                return;
            }
        }
        if n < self.inners.len() {
            self.inners[n].id.set(id);
            self.inners[n].value.set(value);
            self.count.set(n + 1);
        }
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

/// Everything the VST3 owner thread carries out of setup. All COM objects stay on the owner
/// thread except `processor` (moved to the RT thread — `Send` for VST3 interfaces). `hostapp`
/// + `host_ctx` are held only to keep the host context alive until `terminate`; `module` is
/// unloaded last (after every COM object releases) to avoid unmapping live vtbl code.
struct Vst3Setup {
    module: Vst3Module,
    factory: ComPtr<IPluginFactory>,
    component: ComPtr<IComponent>,
    hostapp: ComWrapper<LfHostApp>,
    host_ctx: ComPtr<FUnknown>,
    processor: ComPtr<IAudioProcessor>,
    shared_ptr: usize,
    cap_frames: u32,
    shared_buf: SharedBufferHandle,
    /// What the load-time `activate_component` negotiated (channel counts size the RT buffers).
    activation: Activation,
    period_frames: u32,
    max_frames: u32,
    device_rate: f64,
    name: String,
    /// The plugin's edit controller (P10.2): single-component plugins expose it on the
    /// IComponent (`.cast`), separated-component plugins (JUCE/Surge VST3) host it as a distinct
    /// class created via the factory. None = the plugin has no controller (editor replies Err).
    controller: Option<ComPtr<IEditController>>,
    /// True if `controller` is a distinct object (separated-component) → it needs its own
    /// `terminate()` at teardown. False for single-component (shares the IComponent object).
    controller_separated: bool,
}

/// Owner-local VST3 editor state. `Open` carries everything that must outlive the attached
/// view: the `IPlugView` (released LAST, after `removed()`), our `IPlugFrame` +
/// `IComponentHandler` `ComWrapper`s (the plugin holds raw ptrs to them, so they must not drop
/// while the editor is open), and the host window (RAII `DestroyWindow` on drop). Field order
/// is load-bearing: drop runs view → frame → handler → win, and `removed()` is called first.
enum Vst3Editor {
    Closed,
    Open {
        view: ComPtr<IPlugView>,
        _frame: ComWrapper<LfPlugFrame>,
        _handler: ComWrapper<LfComponentHandler>,
        win: HostWindow,
    },
}

/// Open the VST3 editor on the owner thread. `host_hwnd` (0 = unknown) is the main window, used
/// as the host editor window's owner. Requires a single-component controller (Surge); None →
/// clear Err (separated-component editor is deferred to P10.3). Mirrors the CLAP `editor_open`
/// embedded path: createView → isPlatformTypeSupported(HWND) → setComponentHandler → setFrame →
/// getSize → host window → attached → onSize → show. `kResultTrue == 0`, so compare with `==`.
fn vst3_editor_open(
    controller: &Option<ComPtr<IEditController>>,
    host_hwnd: usize,
    event_tx: Arc<Mutex<Producer<PluginEvent>>>,
    slot: u8,
    window: tauri::WebviewWindow,
    restart: Arc<RestartFlags>,
) -> Result<Vst3Editor, String> {
    let ctl = controller.as_ref().ok_or_else(|| {
        "VST3 plugin exposes no IEditController (separated-component → P10.3)".to_string()
    })?;
    // SAFETY: every call runs on the owner thread; `ctl` is a live IEditController for the
    // loaded plugin, and all raw pointers handed across the FFI are valid for their call.
    unsafe {
        // 1. Minimal host handler, set BEFORE createView (kept alive in Vst3Editor::Open so the
        //    plugin's raw ptr stays valid for the editor session — JUCE edits go inert without it).
        let handler = ComWrapper::new(LfComponentHandler {
            event_tx,
            slot,
            window,
            restart,
        });
        if let Some(hp) = handler.to_com_ptr::<IComponentHandler>() {
            ctl.setComponentHandler(hp.as_ptr());
        }
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
            _handler: handler,
            win,
        })
    }
}

/// Close the VST3 editor: `removed()` detaches the plugin's child from our window FIRST (while
/// the view is still live), THEN the drop runs view-release → frame/handler-release →
/// `HostWindow::drop` (DestroyWindow). Mirrors the CLAP `gui.destroy` → DestroyWindow order.
fn vst3_editor_close(ed: &mut Vst3Editor) {
    if let Vst3Editor::Open { view, .. } = ed {
        // SAFETY: owner thread; detach the plugin's child before our window is destroyed.
        unsafe {
            let _ = view.removed();
        }
    }
    *ed = Vst3Editor::Closed; // drops view/frame/handler then HostWindow::drop → DestroyWindow
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
/// INACTIVE component with no RT producer running; a failure leaves it inactive.
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

/// The per-load constants every RT spawn of this slot shares (all `Copy`; the owner keeps them).
#[derive(Clone, Copy)]
struct Vst3RtConfig {
    slot: u8,
    shared_ptr: usize,
    cap_frames: u32,
    max_frames: u32,
    sample_rate: f64, // C (ctx rate)
    device_rate: f64, // D (render rate)
}

/// What the VST3 RT producer returns on exit: the processor handle (the owner deactivates,
/// re-activates and hands it to the next spawn), the rings, and the state a respawn continues from
/// — the block it last reconciled to and the hop-1 drift it learned. A started processor has already
/// received `setProcessing(0)`; a processor whose `setProcessing(1)` failed is returned untouched.
struct Vst3RtExit {
    processor: ComPtr<IAudioProcessor>,
    rings: RtRings,
    period_frames: u32,
    block_config_gen: u32,
    drift_ppm: f64,
}

/// Spawn the VST3 RT producer with its own run flag (independent of the owner's `running`, so the
/// owner can stop and respawn it mid-load). On spawn failure the processor handle and rings are
/// lost with the closure; the caller leaves the slot silent and logs.
#[allow(clippy::too_many_arguments)]
fn spawn_vst3_rt(
    cfg: &Vst3RtConfig,
    processor: ComPtr<IAudioProcessor>,
    rings: RtRings,
    activation: Activation,
    period_frames: u32,
    block_config_gen: u32,
    drift_ppm: f64,
    diag: Arc<ProducerDiag>,
) -> Result<RtJoinGuard<Vst3RtExit>, String> {
    let rt_run = Arc::new(AtomicBool::new(true));
    let run = rt_run.clone();
    let c = *cfg;
    let join = std::thread::Builder::new()
        .name(format!("lf-vst3-rt-{}", cfg.slot))
        .spawn(move || -> Vst3RtExit {
            vst3_producer_loop(
                processor,
                c.shared_ptr,
                c.cap_frames,
                diag,
                run,
                activation.out_channels,
                activation.in_channels, // P11.2: 0 ⇒ keep numInputs=0 (synth)
                period_frames,
                block_config_gen,
                c.max_frames,
                c.sample_rate,
                c.device_rate,
                drift_ppm,
                rings,
            )
        })
        .map_err(|e| format!("failed to spawn VST3 RT thread: {e}"))?;
    Ok(RtJoinGuard::new(rt_run, join))
}

/// Plugin-requested restart (`restartComponent` with a cycle flag), on the owner thread: stop +
/// join the RT producer (it calls `setProcessing(0)` on its way out if processing started and hands
/// back the processor and the rings), `setActive(0)`, re-run the load-time `activate_component`
/// (buses renegotiated, `setupProcessing` at the same D / max block, `setActive(1)`), then respawn
/// at the block and drift the joined producer reached. `activation` is updated to what the plugin
/// now reports, so a `kIoChanged` that changed a channel count resizes the next producer's buffers
/// instead of feeding the plugin a stale `numChannels`. The editor and native cpal streams stay up.
/// Flags the plugin raises DURING the cycle (a `kLatencyChanged` from inside `setActive(1)` is
/// common) describe the state just activated and are consumed, so a plugin cannot keep the host
/// cycling. A failed re-activation leaves the slot SILENT but still serviced by the owner loop,
/// so unload/reload work normally — the error is logged.
fn service_vst3_restart(
    flags: i32,
    component: &ComPtr<IComponent>,
    rt_guard: &mut Option<RtJoinGuard<Vst3RtExit>>,
    cfg: &Vst3RtConfig,
    activation: &mut Activation,
    restart: &RestartFlags,
    diag: &Arc<ProducerDiag>,
) -> Result<(), String> {
    let guard = rt_guard
        .take()
        .ok_or_else(|| "no RT producer to restart (an earlier restart failed)".to_string())?;
    let result = (|| {
        let exit = guard
            .stop_and_join()
            .map_err(|_| "RT thread panicked during the restart join".to_string())?;
        // SAFETY: owner thread; the producer is joined (no process() in flight), so the
        // deactivate → activate sequence runs on an idle component.
        let before = *activation;
        let reactivated = unsafe {
            if component.setActive(0) != kResultOk {
                return Err("setActive(false) failed".to_string());
            }
            activate_component(component, &exit.processor, cfg.device_rate, cfg.max_frames)
        }?;
        if reactivated != before {
            log::info!(
                "[plugin_host] slot {} the plugin's layout changed across the restart ({}): out {}→{} in {}→{} latency {}→{}",
                cfg.slot,
                restart_flag_names(flags),
                before.out_channels,
                reactivated.out_channels,
                before.in_channels,
                reactivated.in_channels,
                before.latency_frames,
                reactivated.latency_frames
            );
        }
        *activation = reactivated;
        let guard = spawn_vst3_rt(
            cfg,
            exit.processor,
            exit.rings,
            reactivated,
            exit.period_frames,
            exit.block_config_gen,
            exit.drift_ppm,
            diag.clone(),
        )?;
        *rt_guard = Some(guard);
        Ok(())
    })();
    let raised_during = restart.take();
    if raised_during != 0 {
        log::info!(
            "[plugin_host] slot {} restartComponent({}) raised during the cycle describes the state just activated; consumed",
            cfg.slot,
            restart_flag_names(raised_during)
        );
    }
    result
}

/// The VST3 owner thread (clack's "main thread" analogue): loads the module, creates +
/// initializes the `!`-pinned COM objects, activates buses, provisions the hop-1 SharedBuffer,
/// spawns the RT producer (handing it the `Send` processor), emits the gate every ~2s, and on
/// shutdown joins the RT thread, deactivates + terminates, and unloads the DLL last.
#[allow(clippy::too_many_arguments)]
pub fn vst3_owner_main(
    window: tauri::WebviewWindow,
    path: String,
    id: String,
    sample_rate: f64,
    frontend_epoch: u32,
    load_token: u32,
    load_gen: u32,
    running: Arc<AtomicBool>,
    diag: Arc<ProducerDiag>,
    ready_tx: std::sync::mpsc::SyncSender<LoadReady>,
    slot: u8,
    event_rx: Consumer<PluginEvent>,
    request_rx: std::sync::mpsc::Receiver<OwnerRequest>,
    // P10.3: the SAME producer the command threads push notes/params to (Arc<Mutex<>> serialises
    // them into the one SPSC ring). The owner thread needs it so a hosted editor's
    // IComponentHandler::performEdit can forward knob moves to the RT param relay.
    event_tx: Arc<Mutex<Producer<PluginEvent>>>,
    // P11.0: cpal→RT audio-input ring. `in_rx` (Consumer) crosses to the RT producer; the
    // owner-local `input_producer` (Producer behind Arc<Mutex>) feeds successive cpal streams.
    in_rx: Consumer<f32>,
    input_producer: Arc<Mutex<Producer<f32>>>,
    // P11.3 Stage B: native cpal-out monitor (mirrors CLAP owner_main). `mon_tx` (Producer)
    // moves into the RT producer (pushes wet mono); `mon_rx` (Consumer) is wrapped Arc<Mutex>
    // here for the cpal-out callback (successive arm/disarm streams reuse it); `monitor_gain`
    // is the SAME Arc the SlotHandle holds, read each callback (no stream rebuild on a slider).
    mon_tx: Producer<f32>,
    mon_rx: Consumer<f32>,
    monitor_gain: Arc<AtomicU32>,
    // The known param ids (`ParamIds`): filled before the load is reported, refreshed on every
    // enumeration; the SlotHandle holds the same Arc for `set_param`'s check.
    param_ids: ParamIds,
) {
    // Capture before setup reads CHOSEN_BLOCK_FRAMES. Any setting change during the foreign-plugin
    // setup window leaves a generation mismatch for the RT loop to reconcile.
    let block_config_gen_at_load = super::BLOCK_CONFIG_GEN.load(Acquire);
    let setup = (|| -> Result<Vst3Setup, String> {
        let binary =
            super::super::scan::resolve_vst3_binary(std::path::Path::new(&path))
                .ok_or_else(|| format!("no loadable VST3 binary inside {path}"))?;
        let target_cid = super::super::scan::hex_to_tuid(&id)
            .ok_or_else(|| format!("bad VST3 class id: {id}"))?;
        let wide: Vec<u16> = binary
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        // SAFETY: FFI module load + raw FUnknown COM. The bundle was vetted by the
        // out-of-process scan; every pointer below is valid for its call.
        unsafe {
            let module = Vst3Module::load(PCWSTR(wide.as_ptr()))?;
            let gpf = GetProcAddress(module.handle(), s!("GetPluginFactory"))
                .ok_or_else(|| "GetPluginFactory not exported".to_string())?;
            let get_factory: unsafe extern "system" fn() -> *mut IPluginFactory =
                std::mem::transmute(gpf);
            let factory = ComPtr::from_raw(get_factory())
                .ok_or_else(|| "GetPluginFactory returned null".to_string())?;

            // Find the requested class (by hex TUID) and create its IComponent.
            let count = factory.countClasses();
            let mut component: Option<ComPtr<IComponent>> = None;
            let mut name = id.clone();
            for i in 0..count {
                let mut info: PClassInfo = std::mem::zeroed();
                if factory.getClassInfo(i, &mut info) != kResultOk {
                    continue;
                }
                if info.cid != target_cid {
                    continue;
                }
                name = super::super::scan::c_chars_to_string(&info.name);
                let mut obj: *mut c_void = std::ptr::null_mut();
                let r = factory.createInstance(
                    info.cid.as_ptr(),
                    vst3::Steinberg::Vst::IComponent_iid.as_ptr(),
                    &mut obj,
                );
                if r == kResultOk && !obj.is_null() {
                    component = ComPtr::from_raw(obj as *mut IComponent);
                }
                break;
            }
            let component = component
                .ok_or_else(|| format!("class {id} not found / createInstance failed"))?;

            // Host context (mandatory for many plugins) → initialize the component.
            let hostapp = ComWrapper::new(LfHostApp);
            let host_ctx = hostapp
                .to_com_ptr::<FUnknown>()
                .ok_or_else(|| "host app FUnknown failed".to_string())?;
            if component.initialize(host_ctx.as_ptr()) != kResultOk {
                // Not yet active; route through `teardown` anyway for one ordered cleanup path
                // (terminate on a just-initialized component is valid; FreeLibrary runs last).
                teardown(component, host_ctx, hostapp, factory, module);
                return Err("component.initialize failed".to_string());
            }

            // From here the component is initialized (and shortly active): ANY failure must
            // deactivate + terminate + FreeLibrary IN ORDER, or we leak the DLL mapping + an
            // un-terminated plugin (P10.1 review 🟡-1). Run the fallible rest in an inner
            // closure and route its Err through `teardown`; the happy-path calls are unchanged.
            type RestOk = (
                ComPtr<IAudioProcessor>,
                usize,                                              // shared_ptr
                SharedBufferHandle,                                 // shared_buf
                Activation,                                         // negotiated buses + latency
                u32,                                                // period_frames
                u32,                                                // max_frames
                f64,                                                // device_rate (D)
            );
            let rest: Result<RestOk, String> = (|| {
                let processor = component
                    .cast::<IAudioProcessor>()
                    .ok_or_else(|| "plugin has no IAudioProcessor".to_string())?;

                // C = ctx rate; D = render rate (DEV forces a mismatch so the resampler + drift
                // gate are exercised — B3). The plugin renders D-frame blocks; Hop1Pipe → C.
                let c = sample_rate;
                let d = force_device_rate().unwrap_or(c);
                let wasapi_default = wasapi_period_frames(d).unwrap_or_else(|e| {
                    log::warn!("[plugin_host] WASAPI period query failed ({e}); 10ms fallback");
                    ((d * 0.01).round() as u32).max(64)
                });
                // P11.3 live buffer (see CLAP path for the rationale): explicit pick wins; the
                // ASIO 256 cap is the DEFAULT only — an explicit pick is honored up to
                // MAX_SELECTABLE_BLOCK. child mod → super:: for the parent-level consts.
                let chosen = super::CHOSEN_BLOCK_FRAMES.load(Relaxed);
                let mut period_frames = if chosen != 0 { chosen } else { wasapi_default };
                // Cap the DEFAULT block on an AVAILABLE ASIO device (see CLAP path: gate on the
                // startup-fixed asio_available(), not the runtime use_asio(), so load-time cap and
                // arm-time setpoint can't skew). Explicit picks bypass it.
                if chosen == 0 && crate::audio_output::asio_available() {
                    period_frames = period_frames.min(super::ASIO_MAX_BLOCK_FRAMES);
                }
                let period_frames = period_frames.min(super::MAX_SELECTABLE_BLOCK);
                // Roomy activation: max_frames_count = selectable max + headroom so the live block
                // can sweep up to MAX_SELECTABLE_BLOCK without re-running setupProcessing (Task 11).
                let max_frames = super::MAX_SELECTABLE_BLOCK + 128;

                // The one activation sequence (buses → setupProcessing → setActive), shared with
                // the plugin-requested restart cycle.
                let activation = activate_component(&component, &processor, d, max_frames)?;
                if !running.load(Acquire) {
                    return Err("plugin load cancelled by frontend reload".to_string());
                }

                // meta.sampleRate = C: hop-1 frames are post-resample (ctx rate).
                let (shared_ptr, shared_buf) = create_shared_ring(
                    &window,
                    HOP1_CAPACITY_FRAMES,
                    slot,
                    frontend_epoch,
                    load_token,
                    &running,
                    c,
                    activation.in_channels,
                )?;
                let target_frames = (TARGET_FILL_SECONDS * c).round() as u32;
                diag.init(
                    c,
                    d,
                    max_frames,
                    activation.out_channels,
                    HOP1_CAPACITY_FRAMES as usize,
                    target_frames,
                    load_gen,
                );
                diag.block_frames.store(period_frames, Relaxed); // P11.3: active RT block (gate reads it)

                Ok((processor, shared_ptr, shared_buf, activation, period_frames, max_frames, d))
            })();

            let (processor, shared_ptr, shared_buf, activation, period_frames, max_frames, d) =
                match rest {
                    Ok(v) => v,
                    Err(e) => {
                        teardown(component, host_ctx, hostapp, factory, module);
                        return Err(e);
                    }
                };

            // VST3 editor controller (P10.2). On-PC finding: Surge XT VST3 is SEPARATED-
            // component (`component.cast::<IEditController>()` is None — IComponent and
            // IEditController are distinct classes, as JUCE builds them), contra the research's
            // single-component assumption. `obtain_controller` handles both: cast for single,
            // getControllerClassId → createInstance → initialize for separated, reporting which.
            let (controller, controller_separated) =
                obtain_controller(&factory, &component, &host_ctx);

            Ok(Vst3Setup {
                module,
                factory,
                component,
                hostapp,
                host_ctx,
                processor,
                shared_ptr,
                cap_frames: HOP1_CAPACITY_FRAMES,
                shared_buf,
                activation,
                period_frames,
                max_frames,
                device_rate: d,
                name,
                controller,
                controller_separated,
            })
        }
    })();

    let Vst3Setup {
        module,
        factory,
        component,
        hostapp,
        host_ctx,
        processor,
        shared_ptr,
        cap_frames,
        shared_buf,
        activation,
        period_frames,
        max_frames,
        device_rate,
        name,
        controller,
        controller_separated,
    } = match setup {
        Ok(s) => s,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    let info = PluginInfo {
        slot,
        descriptor: PluginDescriptor {
            id,
            name,
            format: "vst3".to_string(),
            path,
            is_effect: None,
        },
    };
    // Before the load is reported, so the frontend's first `setParameter` finds its ids.
    publish_param_ids(&param_ids, &list_vst3_params(&controller).unwrap_or_default());

    // Spawn the RT producer (moves the Send processor handle + the rings). Its run flag is its
    // own, so a plugin-requested restart can stop and respawn it without touching the owner's.
    let rt_cfg = Vst3RtConfig {
        slot,
        shared_ptr,
        cap_frames,
        max_frames,
        sample_rate,
        device_rate,
    };
    let rings = RtRings {
        event_rx,
        in_rx,
        mon_tx,
    };
    // `Option` because a plugin-requested restart takes the guard out, joins, and puts a fresh one
    // back (`service_vst3_restart`); `None` after a failed restart = a silent, still-unloadable slot.
    // The load is reported only after the producer spawned (`spawn_rt_then_ready`).
    let mut rt_guard = spawn_rt_then_ready(
        &ready_tx,
        (info, shared_buf),
        || {
            spawn_vst3_rt(
                &rt_cfg,
                processor,
                rings,
                activation,
                period_frames,
                block_config_gen_at_load,
                0.0, // a fresh load learns the drift from zero
                diag.clone(),
            )
        },
        // Nothing writes the mapping: the producer never started.
        |(_, shared_buf)| shared_buf.close(&window, slot),
    );
    if rt_guard.is_none() {
        // Release the edit controller BEFORE teardown's FreeLibrary — its Release (and, for a
        // separated controller, terminate) vtbl code lives in the plugin DLL, so dropping it
        // AFTER FreeLibrary calls into unmapped code (UAF). Mirror the happy-path teardown's
        // controller handling so this error arm honors the same FreeLibrary-LAST invariant.
        // (Bug-hunt 2026-06-21, #2.)
        if let Some(ctl) = controller {
            if controller_separated {
                // SAFETY: owner thread; no editor view was ever opened on this path. Disconnect
                // the connection points before terminating, then terminate the separate controller.
                unsafe {
                    if let (Some(comp_cp), Some(ctrl_cp)) = (
                        component.cast::<IConnectionPoint>(),
                        ctl.cast::<IConnectionPoint>(),
                    ) {
                        let _ = comp_cp.disconnect(ctrl_cp.as_ptr());
                        let _ = ctrl_cp.disconnect(comp_cp.as_ptr());
                    }
                    let _ = ctl.terminate();
                }
            }
        }
        teardown(component, host_ctx, hostapp, factory, module);
        return;
    }
    diag.alive.store(true, Relaxed);
    let has_input = activation.in_channels > 0;
    let mut native_io = NativeIo::new(
        slot,
        "VST3",
        window.clone(),
        diag.clone(),
        has_input,
        input_producer,
        mon_rx,
        monitor_gain,
    );

    // Give the controller its host handler for the WHOLE load, not only while an editor is open:
    // a plugin loads presets and reports `restartComponent`/`performEdit` from its own state path
    // too (before any editor exists). Kept alive here; the controller add-refs it (SDK + JUCE), so
    // an editor session re-setting its own handler is a normal replace, not a dangling pointer.
    let restart = Arc::new(RestartFlags::default());
    let _load_handler = ComWrapper::new(LfComponentHandler {
        event_tx: event_tx.clone(),
        slot,
        window: window.clone(),
        restart: restart.clone(),
    });
    if let (Some(ctl), Some(hp)) = (
        controller.as_ref(),
        _load_handler.to_com_ptr::<IComponentHandler>(),
    ) {
        // SAFETY: owner thread; `ctl` is the live, initialised IEditController of this load.
        unsafe {
            ctl.setComponentHandler(hp.as_ptr());
        }
    }

    // Owner loop (P10.2): emit the gate every ~2s, service editor open/close, and — while a
    // hosted editor is open — pump Win32 messages (else the embedded view freezes) + react to
    // its close box. Mirrors the CLAP `owner_main` editor loop; VST3 is ALWAYS embedded into our
    // host window (no plugin-owned floating concept), so there's no floating-ack branch. Other
    // control requests (state/params) still reply "unsupported" (P10.3). Audio notes/params ride
    // the event ring, not this channel.
    let host_hwnd: usize = window.hwnd().map(|h| h.0 as usize).unwrap_or(0);
    let mut editor = Vst3Editor::Closed;
    #[cfg(debug_assertions)]
    let mut gate_state = super::GateState::new();
    let emit_period = Duration::from_millis(2000);
    let mut last_emit = Instant::now();
    let mut reported_rt_faults = 0u32;
    let mut activation = activation;
    while running.load(Relaxed) {
        // The plugin's restartComponent reports, drained on the owner thread (the callback only
        // raised flags; it may have run on a foreign thread). A cycle flag = the plugin asked for
        // a deactivate → activate cycle.
        let flags = restart.take();
        if flags != 0 {
            log::info!(
                "[plugin_host] slot {slot} VST3 restartComponent({})",
                restart_flag_names(flags)
            );
            match service_vst3_restart(
                flags,
                &component,
                &mut rt_guard,
                &rt_cfg,
                &mut activation,
                &restart,
                &diag,
            ) {
                Ok(()) => log::info!(
                    "[plugin_host] slot {slot} restarted at the plugin's request ({}: setActive(0) → setupProcessing → setActive(1) → RT resumed; out={} in={} latency={})",
                    restart_flag_names(flags),
                    activation.out_channels,
                    activation.in_channels,
                    activation.latency_frames
                ),
                Err(e) => log::error!("[plugin_host] slot {slot} plugin-requested restart failed: {e}"),
            }
            if has_input != (activation.in_channels > 0) {
                log::warn!(
                    "[plugin_host] slot {slot} the plugin's audio-input bus presence changed across the restart (load-time in={} now in={}); native input arming and the web UI's synth/effect kind still follow the load-time layout — unload and reload the plugin to pick the new one up",
                    if has_input { "yes" } else { "no" },
                    activation.in_channels
                );
            }
        }
        // After the cycle, so a re-list the plugin raised during its re-activation lands this turn.
        let notices = restart.take_notify();
        if notices != 0 {
            log::info!(
                "[plugin_host] slot {slot} VST3 restartComponent({})",
                restart_flag_names(notices)
            );
            if notices & RestartFlags::RELIST != 0 {
                publish_param_ids(&param_ids, &list_vst3_params(&controller).unwrap_or_default());
                let _ = window.emit("plugin:params-changed", slot);
            }
        }
        // (a) Hosted editor: pump our window's messages, then react to the user's close box.
        if matches!(editor, Vst3Editor::Open { .. }) {
            pump_thread_messages();
            let user_closed = match &editor {
                Vst3Editor::Open { win, .. } => win.close_requested(),
                _ => false,
            };
            if user_closed {
                vst3_editor_close(&mut editor); // removed() → drop → DestroyWindow → pump-drain
                let _ = window.emit("plugin:editor-closed", slot);
                log::info!("[plugin_host] slot {slot} VST3 editor closed by user");
            }
        }
        // (b) Service one request: hosted → poll briefly (keep the UI responsive); else block
        //     until the next request or the gate deadline.
        let req = if matches!(editor, Vst3Editor::Open { .. }) {
            wait_for_input(20);
            request_rx.try_recv().ok()
        } else {
            let timeout = emit_period.saturating_sub(last_emit.elapsed());
            match request_rx.recv_timeout(timeout) {
                Ok(r) => Some(r),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    std::thread::sleep(Duration::from_millis(10));
                    None
                }
            }
        };
        if let Some(req) = req.and_then(|r| super::take_uncancelled(r, slot)) {
            match req {
                OwnerRequest::OpenEditor(cancelled, reply) => {
                    let was_closed = matches!(editor, Vst3Editor::Closed);
                    let res = if was_closed {
                        match vst3_editor_open(
                            &controller,
                            host_hwnd,
                            event_tx.clone(),
                            slot,
                            window.clone(),
                            restart.clone(),
                        ) {
                            Ok(ed) => {
                                editor = ed;
                                Ok(())
                            }
                            Err(e) => Err(e),
                        }
                    } else {
                        Ok(()) // already open — idempotent
                    };
                    // A view that took >5s to come up is a LATE success: the caller reported
                    // failure and JS shows no editor, so close it again through the CloseEditor
                    // path. Only what THIS request opened — an editor that was already open
                    // belongs to an earlier successful open. Mirror of CLAP owner_main.
                    if was_closed && res.is_ok() && cancelled.load(Relaxed) {
                        log::warn!("[plugin_host] slot {slot} openEditor: owner-request cancelled mid-call (caller timed out) — rolling back, closing the editor");
                        vst3_editor_close(&mut editor);
                    }
                    let _ = reply.send(res);
                }
                // No rollback: a late close lands where both sides converge. A CANCELLED close never
                // reaches here — it is skipped before it starts (`take_uncancelled`).
                OwnerRequest::CloseEditor(_, reply) => {
                    if !matches!(editor, Vst3Editor::Closed) {
                        vst3_editor_close(&mut editor);
                    }
                    let _ = reply.send(Ok(()));
                }
                OwnerRequest::ListParams(reply) => {
                    // Enumerate on the owner thread (the !Send controller is pinned here). The UI
                    // + any host-originated setParameter use these stable ids (Surge's are
                    // hash-like, not 0-based — never invent one).
                    let res = list_vst3_params(&controller);
                    if let Ok(params) = &res {
                        publish_param_ids(&param_ids, params);
                    }
                    let _ = reply.send(res);
                }
                OwnerRequest::SetParamNormalized(id, value) => {
                    // Host contract: the processor got this value through the ring; the controller
                    // (GUI state, and the side that raises a controller-decided restartComponent)
                    // is told here, on its own thread. A restart flag it raises inside the call
                    // lands on `restart` and is serviced at the top of the next turn.
                    if let Some(ctl) = controller.as_ref() {
                        // SAFETY: owner thread, live controller.
                        let r = unsafe { ctl.setParamNormalized(id as ParamID, value) };
                        if r != kResultOk {
                            log::debug!("[plugin_host] slot {slot} controller.setParamNormalized({id}) → {r:#x}");
                        }
                    }
                }
                OwnerRequest::ArmInput(device_id, channel, cancelled, reply) => {
                    let res = native_io.arm_input(
                        device_id.as_deref(),
                        channel,
                        cancelled.as_ref(),
                    );
                    let _ = reply.send(res);
                }
                // A cancelled disarm is neither skipped nor rolled back: JS already reconciled to
                // "gone" before it awaited, so running late is what makes the two sides agree
                // (`take_uncancelled`). The token is ignored here on purpose.
                OwnerRequest::DisarmInput(_, reply) => {
                    let _ = reply.send(native_io.disarm_input());
                }
                OwnerRequest::ArmMonitor(device_id, cancelled, reply) => {
                    let res = native_io.arm_monitor(device_id.as_deref(), cancelled.as_ref());
                    let _ = reply.send(res);
                }
                // A cancelled disarm is neither skipped nor rolled back — see DisarmInput above.
                OwnerRequest::DisarmMonitor(_, reply) => {
                    let _ = reply.send(native_io.disarm_monitor());
                }
                // An unload's wake-up: nothing to do, the loop condition sees `running=false`.
                OwnerRequest::Wake => {}
                #[cfg(debug_assertions)]
                other => reply_unsupported(other), // DEV state save/load is not wired for VST3
            }
        }
        native_io.poll_faults();
        if last_emit.elapsed() >= emit_period {
            native_io.mirror_diag();
            report_new_rt_faults(&diag, slot, &mut reported_rt_faults);
            #[cfg(debug_assertions)]
            super::emit_gate(&diag, &mut gate_state);
            last_emit = Instant::now();
        }
    }

    // Teardown: close a still-open editor BEFORE deactivate/terminate/FreeLibrary (a live
    // attached view through terminate → crash/leak). Then the RT thread saw running=false →
    // setProcessing(false), if it started, + exited; join it, deactivate + terminate on this
    // (owner) thread,
    // release all module COM objects, unload last.
    // Every step is timed into ONE log line: a plugin that stalls here reads as a frozen app, and
    // the line says which step to blame.
    let t_teardown = Instant::now();
    if !matches!(editor, Vst3Editor::Closed) {
        vst3_editor_close(&mut editor);
    }
    let _ = editor;
    let editor_ms = t_teardown.elapsed().as_millis();
    // Stop native capture + monitor before the RT thread (ring ends) goes away. The joined
    // producer hands back its processor handle, released here (owner thread, module still mapped).
    let t = Instant::now();
    drop(native_io);
    let native_io_ms = t.elapsed().as_millis();
    let t = Instant::now();
    if let Some(guard) = rt_guard.take() {
        let _ = guard.stop_and_join();
    }
    let rt_join_ms = t.elapsed().as_millis();
    report_new_rt_faults(&diag, slot, &mut reported_rt_faults);
    // Release the edit controller BEFORE teardown's FreeLibrary — its Release (and terminate)
    // vtbl code lives in the DLL, so it must drop while the module is still mapped. A separated
    // controller is a distinct object needing its own terminate(); a single-component one shares
    // the IComponent object (teardown's terminate covers it — terminating here would double it).
    let t = Instant::now();
    if let Some(ctl) = controller {
        if controller_separated {
            // SAFETY: owner thread; the only view was closed above. Disconnect the connection
            // points before terminating so neither side notifies a half-torn-down peer, then
            // terminate the separately-created controller.
            unsafe {
                if let (Some(comp_cp), Some(ctrl_cp)) = (
                    component.cast::<IConnectionPoint>(),
                    ctl.cast::<IConnectionPoint>(),
                ) {
                    let _ = comp_cp.disconnect(ctrl_cp.as_ptr());
                    let _ = ctrl_cp.disconnect(comp_cp.as_ptr());
                }
                let _ = ctl.terminate();
            }
        }
    }
    let controller_ms = t.elapsed().as_millis();
    let [deactivate_ms, terminate_ms, release_ms, module_ms] =
        teardown(component, host_ctx, hostapp, factory, module);
    log::info!(
        "[plugin_host] slot {slot} VST3 teardown {} ms: editor={editor_ms} native_io={native_io_ms} rt_join={rt_join_ms} controller={controller_ms} setActive(0)={deactivate_ms} terminate={terminate_ms} release={release_ms} module={module_ms}",
        t_teardown.elapsed().as_millis()
    );
    diag.alive.store(false, Relaxed);
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
    // SAFETY: the RT producer has stopped (joined); deactivate + terminate on the owner thread.
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

/// Reply to a DEV control request the VST3 host doesn't service (state save/load).
#[cfg(debug_assertions)]
fn reply_unsupported(req: OwnerRequest) {
    let msg = || "VST3 state save/load is not wired".to_string();
    match req {
        #[cfg(debug_assertions)]
        OwnerRequest::SaveState(r) => {
            let _ = r.send(Err(msg()));
        }
        #[cfg(debug_assertions)]
        OwnerRequest::LoadState(_, r) => {
            let _ = r.send(Err(msg()));
        }
        OwnerRequest::ListParams(r) => {
            let _ = r.send(Ok(Vec::new()));
        }
        OwnerRequest::SetParamNormalized(..) | OwnerRequest::Wake => {}
        OwnerRequest::OpenEditor(_, r) => {
            let _ = r.send(Err(msg()));
        }
        OwnerRequest::CloseEditor(_, r) => {
            let _ = r.send(Ok(()));
        }
        // P11.0 input requests are handled in the VST3 owner loop's explicit arms; this is only
        // for match exhaustiveness (they never route here).
        OwnerRequest::ArmInput(_, _, _, r) => {
            let _ = r.send(Err(msg()));
        }
        OwnerRequest::DisarmInput(_, r) => {
            let _ = r.send(Ok(()));
        }
        // P11.3 Stage B native monitor: now handled in the VST3 owner loop's explicit arms; this
        // is only for match exhaustiveness (they never route here).
        OwnerRequest::ArmMonitor(_, _, r) => {
            let _ = r.send(Err(msg()));
        }
        OwnerRequest::DisarmMonitor(_, r) => {
            let _ = r.send(Ok(()));
        }
    }
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
        let mut out = Vec::with_capacity(count.max(0) as usize);
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

/// The device-less VST3 RT producer. Renders the plugin at D into per-channel f32 buffers via
/// `IAudioProcessor::process`, sums to mono, and hands each D-rate block to the shared
/// `Hop1Pipe` (resample D→C + hop-1 ring write + pacing). ZERO heap allocation per block after
/// warmup (host COM objects + the resampler are built pre-loop). Calls `setProcessing(1)` on entry.
/// A successful start is paired with `setProcessing(0)` on the way out; a rejected start returns
/// immediately without `process()` or `setProcessing(0)`. It returns the processor + rings
/// (`Vst3RtExit`) when its run flag drops — at unload, or mid-load for a plugin-requested restart.
#[allow(clippy::too_many_arguments)]
fn vst3_producer_loop(
    processor: ComPtr<IAudioProcessor>,
    shared_ptr: usize,
    cap_frames: u32,
    diag: Arc<ProducerDiag>,
    running: Arc<AtomicBool>,
    out_channels: u32,
    in_channels: u32, // P11.2: plugin audio-input channels; 0 = no input bus (keep numInputs=0)
    period_frames: u32,
    block_config_gen_at_load: u32,
    max_frames: u32,
    sample_rate: f64, // C
    device_rate: f64, // D
    drift_ppm: f64,   // the predecessor's learned hop-1 drift (0.0 at load)
    rings: RtRings,
) -> Vst3RtExit {
    let RtRings {
        mut event_rx,
        mut in_rx,
        mut mon_tx,
    } = rings;
    let mmcss = promote_pro_audio();

    let chans = out_channels.max(1) as usize;
    // P11.3 live buffer: period_frames + block are mutable so a config-generation bump re-paces the
    // loop at a new D-block without re-running setupProcessing (buffers stay sized to cap_buf =
    // max_frames; pd.numSamples = block tells the plugin how many frames to process).
    let mut period_frames = period_frames;
    let mut block = period_frames as usize;
    let cap_buf = max_frames as usize;
    let mut out_bufs: Vec<Vec<f32>> = vec![vec![0.0f32; cap_buf]; chans];
    let mut mono: Vec<f32> = vec![0.0f32; cap_buf];
    // Stable per-channel row pointers (out_bufs rows never reallocate) → ProcessData output.
    let mut ch_ptrs: Vec<*mut f32> = out_bufs.iter_mut().map(|c| c.as_mut_ptr()).collect();

    // P11.2: input rows + stable ptrs (only when the plugin has an input bus). Mono capture is
    // duplicated across all input rows; sized to cap_buf (alloc-free per block). `in_chans == 0`
    // (synth) → empty Vecs, and the process block keeps the exact P10 numInputs=0 layout.
    let in_chans = in_channels as usize;
    let mut in_bufs: Vec<Vec<f32>> = if in_chans > 0 {
        vec![vec![0.0f32; cap_buf]; in_chans]
    } else {
        Vec::new()
    };
    let mut in_ch_ptrs: Vec<*mut f32> = in_bufs.iter_mut().map(|c| c.as_mut_ptr()).collect();

    // P11 input-SRC: the cpal→D input resampler, (re)built on each arm/disarm/device-swap
    // (signalled by diag.input_gen). `None` ⇒ feed silence. fill_block writes in_bufs[0] (the
    // memory in_ch_ptrs point at — stable, rows never realloc). The build allocates: an
    // accepted one-shot arm-time cost (outside the rt_alloc guard; not steady-state).
    let mut in_pipe: Option<InPipe> = None;
    let mut in_gen: u32 = 0;
    // P11.3 Stage B: the native monitor pipe (branch-1), (re)built on each monitor
    // arm/disarm/device-swap (`monitor_gen`). `None` = monitor disarmed ⇒ don't push (the cpal
    // output stream is gone). The build allocates → kept OUTSIDE the rt_alloc guard, like in_pipe.
    let mut out_pipe: Option<OutMonitorPipe> = None;
    let mut mon_gen: u32 = 0;
    // P11.3 live buffer: the setup generation snapshot lets the first iteration detect a buffer
    // change that happened while this slot was still loading (parity with the CLAP loop).
    let mut block_config_gen_seen = block_config_gen_at_load;

    // Host COM objects (RT-local; interior-mutable). Built once → alloc-free per block.
    let ev_inner = Rc::new(EventListInner::new());
    let event_list = ComWrapper::new(RtEventList {
        inner: ev_inner.clone(),
    });
    // A pre-loop build or processing-start failure hands everything back without calling
    // setProcessing(0): either setProcessing(1) never ran or it did not succeed.
    macro_rules! bail_before_start {
        ($fault:expr) => {{
            diag.latch_rt_fault($fault);
            if let Some(h) = mmcss {
                revert(h);
            }
            return Vst3RtExit {
                processor,
                rings: RtRings {
                    event_rx,
                    in_rx,
                    mon_tx,
                },
                period_frames,
                block_config_gen: block_config_gen_seen,
                drift_ppm,
            };
        }};
    }
    let event_list_ptr = match event_list.to_com_ptr::<IEventList>() {
        Some(p) => p,
        None => bail_before_start!(RtFault::Vst3Process),
    };
    // Real host param-changes: a pre-built pool of IParamValueQueue objects + their cached raw
    // ptrs (alloc-free per block). pc_inner is mutated each block by the drain; the RtParamChanges
    // COM object reads it via the shared Rc. Both must outlive the loop (they do — loop locals).
    let pc_inner = match ParamChangesInner::new(MAX_PARAM_QUEUES) {
        Some(inner) => Rc::new(inner),
        None => bail_before_start!(RtFault::Vst3Process),
    };
    let param_changes = ComWrapper::new(RtParamChanges {
        inner: pc_inner.clone(),
    });
    let param_changes_ptr = match param_changes.to_com_ptr::<IParameterChanges>() {
        Some(p) => p,
        None => bail_before_start!(RtFault::Vst3Process),
    };

    let mut pipe = match Hop1Pipe::new(
        shared_ptr,
        cap_frames,
        sample_rate,
        device_rate,
        period_frames,
    ) {
        Ok(p) => p,
        Err(_) => bail_before_start!(RtFault::Hop1Rebuild),
    };
    pipe.resume_drift_ppm(drift_ppm);

    // SAFETY: setProcessing on the RT thread per the VST3 call sequence.
    unsafe {
        if processor.setProcessing(1) != kResultOk {
            bail_before_start!(RtFault::Vst3Process);
        }
    }

    let pmode = ProcessModes_::kRealtime as i32;
    let ssize = SymbolicSampleSizes_::kSample32 as i32;
    let mut warmup: u32 = 8;
    while running.load(Relaxed) {
        // P11 input-SRC: (re)build on each arm/disarm/device-swap (gen bump). OUTSIDE the
        // rt_alloc guard (InPipe::new allocates) → steady state stays rt_allocs:0; fires only
        // on a control event. Flush stale ring frames so a (re)arm starts clean. Gating on
        // input_gen (not the rate) means a same-rate device swap still rebuilds + flushes. Acquire
        // pairs the owner's rate + actual-backend publication.
        if in_chans > 0 {
            let gen_now = diag.input_gen.load(Acquire);
            if gen_now != in_gen {
                in_gen = gen_now;
                let rate_now = diag.input_rate.load(Relaxed);
                while in_rx.pop().is_ok() {}
                in_pipe = if rate_now == 0 {
                    None
                } else {
                    let is_asio = diag.input_is_asio.load(Relaxed);
                    match InPipe::new(rate_now, device_rate, period_frames, is_asio) {
                        Ok(p) => Some(p),
                        Err(_) => {
                            diag.latch_rt_fault(RtFault::InputRebuild);
                            None
                        }
                    }
                };
            }
        }
        // P11.3 Stage B: (re)build the monitor pipe on a monitor_gen bump (arm/disarm/device-
        // swap), same discipline as in_pipe — OUTSIDE the rt_alloc guard. rate 0 ⇒ disarmed ⇒ None.
        {
            let gen_now = diag.monitor_gen.load(Acquire);
            if gen_now != mon_gen {
                mon_gen = gen_now;
                let rate_now = diag.monitor_rate.load(Relaxed);
                out_pipe = if rate_now == 0 {
                    None
                } else {
                    let is_asio = diag.monitor_is_asio.load(Relaxed);
                    match OutMonitorPipe::new(
                        rate_now,
                        device_rate,
                        period_frames,
                        OUT_RING_CAP,
                        is_asio,
                    ) {
                        Ok(p) => Some(p),
                        Err(_) => {
                            diag.latch_rt_fault(RtFault::MonitorRebuild);
                            None
                        }
                    }
                };
            }
        }
        // P11.3 live buffer: re-pace to a new D-block on a global config-generation bump (parity
        // with the CLAP loop). Runs AFTER the input/monitor rebuilds so an arm-and-resize in the
        // same tick lands on the final block; does NOT flush in_rx. Child mod → super:: for
        // the parent-level block consts.
        {
            let bg_now = super::BLOCK_CONFIG_GEN.load(Acquire);
            if bg_now != block_config_gen_seen {
                block_config_gen_seen = bg_now;
                let chosen = super::CHOSEN_BLOCK_FRAMES.load(Relaxed);
                let new_period = (if chosen != 0 { chosen } else { period_frames })
                    .min(super::MAX_SELECTABLE_BLOCK);
                // Rebuild ONLY on an actual change (idempotent; see the seed comment).
                if new_period != period_frames {
                    match pipe.rebuild_for_block(device_rate, new_period) {
                        Ok(()) => {
                            period_frames = new_period;
                            block = new_period as usize;
                            diag.block_frames.store(new_period, Relaxed);
                            if let Some(p) = in_pipe.as_mut() {
                                if p.set_block(device_rate, new_period).is_err() {
                                    diag.latch_rt_fault(RtFault::InputRebuild);
                                    in_pipe = None;
                                }
                            }
                            if let Some(p) = out_pipe.as_mut() {
                                if p.set_block(device_rate, new_period).is_err() {
                                    diag.latch_rt_fault(RtFault::MonitorRebuild);
                                    out_pipe = None;
                                }
                            }
                        }
                        Err(_) => diag.latch_rt_fault(RtFault::Hop1Rebuild),
                    }
                }
            }
        }
        {
            #[cfg(debug_assertions)]
            let _g = if warmup == 0 {
                Some(super::super::rt_alloc::guard())
            } else {
                None
            };

            // Drain the main→audio ring (alloc-free; surplus stays for the next block). Notes go
            // to the host event list; Param events (P10.3) go to the host IParameterChanges →
            // process() applies them → the plugin's sound changes.
            ev_inner.clear();
            pc_inner.clear();
            let mut drained = 0usize;
            while drained < MAX_EVENTS_PER_BLOCK {
                match event_rx.pop() {
                    Ok(PluginEvent::Param { id, value }) => {
                        pc_inner.push_param(id as ParamID, value);
                        drained += 1;
                    }
                    Ok(ev) => {
                        if let Some(e) = plugin_event_to_vst3(ev) {
                            ev_inner.push(e);
                        }
                        drained += 1;
                    }
                    Err(_) => break,
                }
            }

            // Zero the output rows so a plugin that reports silence (and skips writing) yields
            // silence, not stale audio. Cheap (chans*block writes), no allocation.
            for ch in out_bufs.iter_mut() {
                for s in ch[..block].iter_mut() {
                    *s = 0.0;
                }
            }

            // P11 input-SRC: produce the mono input row from the cpal ring through the R_in→D
            // resampler (`in_pipe`); disarmed ⇒ silence. Mono row 0 → duplicated to further
            // rows. Skipped for a synth (in_chans==0) so its path is byte-identical to P10
            // (numInputs=0). Alloc-free (resampler + scratch live in InPipe).
            if in_chans > 0 {
                match in_pipe.as_mut() {
                    Some(p) => p.fill_block(&mut in_rx, &mut in_bufs[0], block, warmup == 0, &diag),
                    None => {
                        for s in in_bufs[0][..block].iter_mut() {
                            *s = 0.0;
                        }
                        diag.input_fill.store(0, Relaxed);
                    }
                }
                if in_chans > 1 {
                    let (first, rest) = in_bufs.split_at_mut(1);
                    for r in rest.iter_mut() {
                        r[..block].copy_from_slice(&first[0][..block]);
                    }
                }
            }

            // SAFETY: ProcessData points at our stable output (+ P11.2 input) buffers + the
            // RT-local host COM objects, all valid for the synchronous `process()` call. For a
            // synth in_chans==0 ⇒ numInputs=0 + null inputs (the canonical instrument layout);
            // for an FX, numInputs=1 + the input bus sized to the QUERIED channel count.
            // channelBuffers32 is `*mut *mut f32` into the stable rows.
            unsafe {
                let mut out_bus = AudioBusBuffers {
                    numChannels: chans as i32,
                    silenceFlags: 0,
                    __field0: AudioBusBuffers__type0 {
                        channelBuffers32: ch_ptrs.as_mut_ptr(),
                    },
                };
                let mut in_bus = AudioBusBuffers {
                    numChannels: in_chans as i32,
                    silenceFlags: 0,
                    __field0: AudioBusBuffers__type0 {
                        channelBuffers32: in_ch_ptrs.as_mut_ptr(),
                    },
                };
                let mut pd: ProcessData = std::mem::zeroed();
                pd.processMode = pmode;
                pd.symbolicSampleSize = ssize;
                pd.numSamples = block as i32;
                pd.numInputs = if in_chans > 0 { 1 } else { 0 };
                pd.numOutputs = 1;
                pd.inputs = if in_chans > 0 {
                    &mut in_bus
                } else {
                    std::ptr::null_mut()
                };
                pd.outputs = &mut out_bus;
                pd.inputEvents = event_list_ptr.as_ptr();
                pd.outputEvents = std::ptr::null_mut();
                pd.inputParameterChanges = param_changes_ptr.as_ptr();
                pd.outputParameterChanges = std::ptr::null_mut();
                pd.processContext = std::ptr::null_mut();
                let st = processor.process(&mut pd);
                if st != kResultOk {
                    diag.latch_rt_fault(RtFault::Vst3Process);
                }
            }

            sum_to_mono(&out_bufs, &mut mono, block, chans);
            #[cfg(debug_assertions)]
            crate::marker_probe::inject(Arc::as_ptr(&diag) as usize, &mut mono[..block], device_rate);
            pipe.publish(&mono[..block], warmup == 0, &diag);
            // P11.3 Stage B branch-1: also push the SAME wet mono to the native monitor (armed).
            if let Some(mp) = out_pipe.as_mut() {
                mp.publish(&mono[..block], warmup == 0, &mut mon_tx, &diag);
            }
        }

        if warmup > 0 {
            warmup -= 1;
            if warmup == 0 {
                #[cfg(debug_assertions)]
                super::super::rt_alloc::RT_ALLOCS.store(0, Relaxed);
            }
        }
        pipe.pace();
    }

    // SAFETY: stop processing on the RT thread before the owner deactivates the component.
    unsafe {
        let _ = processor.setProcessing(0);
    }
    if let Some(h) = mmcss {
        revert(h);
    }
    Vst3RtExit {
        processor,
        rings: RtRings {
            event_rx,
            in_rx,
            mon_tx,
        },
        period_frames,
        block_config_gen: block_config_gen_seen,
        drift_ppm: pipe.drift_ppm(),
    }
}

#[cfg(test)]
#[path = "vst3_restart_fixture.rs"]
mod restart_tests;
#[cfg(test)]
#[path = "vst3_resize_fixture.rs"]
mod resize_tests;

/// Audit B5: `restartComponent`'s whole body is `RestartFlags::raise`, so a plugin calling it from
/// its own worker thread must touch nothing but atomics there — no log (a lock + allocation) and no
/// emit (the flags hold no window). The owner's drain then sees every flag, split by kind.
#[cfg(test)]
mod restart_flag_tests {
    use super::*;
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

        let restart = Arc::new(RestartFlags::default());
        let raiser = restart.clone();
        let logged = logs_on_a_foreign_thread(move || {
            raiser.raise(
                RestartFlags_::kParamValuesChanged
                    | RestartFlags_::kLatencyChanged
                    | RestartFlags_::kMidiCCAssignmentChanged,
            );
        });
        assert_eq!(logged, 0, "restartComponent must not log on the plugin's thread");

        // The owner turn: the cycle flag for service_vst3_restart, the rest for the log + re-list.
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
