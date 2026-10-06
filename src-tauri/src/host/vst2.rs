//! VST2 hosting, the half every user of a VST2 plugin shares (the interface itself: `vst2_abi.rs`):
//! the module loader, the per-instance `HostContext` with the host callback that answers through
//! it, and `open_effect`, which creates one instance and refuses any this host cannot run. The scan
//! opens an effect to describe it (`scan.rs`); an engine slot's owner and unit build on the same
//! items.
//!
//! A VST2 plugin calls the host callback from ANY thread: its GUI thread, a thread of its own, and
//! the audio thread from inside `setParameter` or a process call. So the callback obeys the audio
//! path's rules everywhere (invariant 5: no allocation, log, lock or wait), and everything it leaves
//! for the owner is a latch in the instance's `HostContext`, which the owner drains on its own turn.
//! Three thread-locals carry what a call cannot: the instance being created (its `AEffect` does not
//! point at its context yet), the processing unit's time info, and each other thread's own snapshot
//! of the time, so no thread ever reads a time info another thread writes. Two requests are served
//! at once when the caller is the instance's owner thread and not inside a unit's plugin call
//! (`owner_idle`, `owner_size_window`): only that thread may pump messages or touch a window.

use std::cell::{Cell, UnsafeCell};
use std::ffi::c_void;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::atomic::{
    AtomicBool, AtomicI32, AtomicIsize, AtomicU32, AtomicU64,
    Ordering::{AcqRel, Acquire, Relaxed, Release},
};
use std::sync::{Arc, OnceLock};

use windows::core::{s, PCWSTR};
use windows::Win32::Foundation::{FreeLibrary, HMODULE, HWND};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Threading::GetCurrentThreadId;

use super::clap::{checked_plugin_channels, checked_plugin_params};
use super::editor_window::{client_size, pump_thread_messages, set_client_size};
use super::vst2_abi::*;

/// What `audioMasterGetVendorString` and `audioMasterGetProductString` answer. A plugin's buffer
/// for either holds at least 64 bytes; this is written whole, with its terminator.
const HOST_NAME: &[u8] = b"BleepLoop\0";
const _: () = assert!(HOST_NAME.len() <= 64);

/// What `audioMasterGetVendorVersion` answers: the app's version as `major·10000 + minor·100 + patch`.
const HOST_VERSION: isize = decimal(env!("CARGO_PKG_VERSION_MAJOR")) * 10_000
    + decimal(env!("CARGO_PKG_VERSION_MINOR")) * 100
    + decimal(env!("CARGO_PKG_VERSION_PATCH"));

const fn decimal(text: &str) -> isize {
    let bytes = text.as_bytes();
    let (mut value, mut i) = (0isize, 0);
    while i < bytes.len() {
        value = value * 10 + (bytes[i] - b'0') as isize;
        i += 1;
    }
    value
}

/// The `canDo` strings this host answers 1 to; every other is 0.
const HOST_CAN_DO: [&[u8]; 4] = [b"sendVstEvents", b"sendVstMidiEvent", b"sizeWindow", b"startStopProcess"];

/// The largest latency a plugin may report, in frames (87 s at 192 kHz): past any real plugin's,
/// and far below what a garbage field holds.
const MAX_INITIAL_DELAY: i32 = 1 << 24;

/// The rate a time info carries when its context holds none that is usable.
const FALLBACK_RATE: f64 = 44_100.0;

/// Owns one loaded VST2 DLL and its entry. The module must outlive every effect its entry made:
/// drop it after the last `Vst2Effect::close`, or `leak` it with an instance that was never closed.
pub(crate) struct Vst2Module {
    handle: HMODULE,
    entry: EntryFn,
}

impl Vst2Module {
    /// Load the DLL at `path` and resolve its entry: `VSTPluginMain`, and only when the module does
    /// not export that, `main` (what plugins older than VST 2.4 export). Loading runs the DLL's own
    /// initialisation code.
    pub(crate) fn load(path: &Path) -> Result<Self, String> {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is a NUL-terminated path that outlives the call. Loading runs foreign
        // code: the scan does it in its throwaway child, an owner only for a plugin the scan vetted.
        let handle = unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) }.map_err(|e| format!("LoadLibraryW: {e}"))?;
        // SAFETY: `handle` is the module just loaded; the names are NUL-terminated literals.
        let found = unsafe { GetProcAddress(handle, s!("VSTPluginMain")).or_else(|| GetProcAddress(handle, s!("main"))) };
        match found {
            // SAFETY: a VST2 module's entry has this signature; nothing else is ever resolved here.
            Some(entry) => Ok(Self { handle, entry: unsafe { std::mem::transmute::<_, EntryFn>(entry) } }),
            None => {
                // SAFETY: the module reference taken above, released once.
                let _ = unsafe { FreeLibrary(handle) };
                Err("no VST2 entry (VSTPluginMain or main) is exported".to_string())
            }
        }
    }

    /// The module's entry, for `open_effect`.
    pub(crate) fn entry(&self) -> EntryFn {
        self.entry
    }

    /// Keep the DLL mapped for the rest of the process: for a module with an instance that was
    /// never closed (a refused effect, a unit the engine did not hand back).
    pub(crate) fn leak(self) {
        std::mem::forget(self);
    }
}

impl Drop for Vst2Module {
    fn drop(&mut self) {
        // SAFETY: the module reference `load` took, released once; the owner closed every effect
        // of this module first (or leaked the module instead of dropping it).
        let _ = unsafe { FreeLibrary(self.handle) };
    }
}

/// Parameter automation a plugin reports (`audioMasterAutomate`), kept without a lock and without
/// loss between any number of reporting threads: the value per parameter and a bit per parameter
/// that says it moved. A report stores the value, THEN sets the bit; the drain clears a word's bits,
/// THEN reads the values, so a value stored after its bit was taken sets the bit again.
pub(crate) struct AutomationLatch {
    /// Each parameter's last reported value (`f32` bits).
    values: Box<[AtomicU32]>,
    dirty: Box<[AtomicU64]>,
}

impl AutomationLatch {
    fn new(params: usize) -> Self {
        Self {
            values: (0..params).map(|_| AtomicU32::new(0)).collect(),
            dirty: (0..params.div_ceil(64)).map(|_| AtomicU64::new(0)).collect(),
        }
    }

    /// Any thread, the audio thread included. An index the plugin never declared is ignored.
    fn record(&self, index: i32, value: f32) {
        let Some(slot) = usize::try_from(index).ok().and_then(|i| self.values.get(i).map(|v| (i, v))) else {
            return;
        };
        slot.1.store(value.to_bits(), Release);
        self.dirty[slot.0 / 64].fetch_or(1 << (slot.0 % 64), AcqRel);
    }

    /// Owner thread: hand every parameter reported since the last drain to `each`, with its latest
    /// value. A value is never echoed back to the plugin and never fetched with `getParameter`.
    pub(crate) fn drain(&self, mut each: impl FnMut(usize, f32)) {
        for (word, bits) in self.dirty.iter().enumerate() {
            let mut set = bits.swap(0, AcqRel);
            while set != 0 {
                let index = word * 64 + set.trailing_zeros() as usize;
                set &= set - 1;
                each(index, f32::from_bits(self.values[index].load(Acquire)));
            }
        }
    }
}

/// One plugin instance's side of the host, shared by its owner, its unit and the host callback. It
/// is created BEFORE the plugin's entry runs and must outlive the plugin: its owner keeps it through
/// `effClose` and the module's unload, and leaks it together with a module it leaks. Atomics only,
/// plus the automation latch, allocated once when the effect is accepted.
pub(crate) struct HostContext {
    /// `f64` bits.
    sample_rate: AtomicU64,
    block_size: AtomicI32,
    /// Frames the unit has processed, for the time a non-processing thread asks for.
    sample_pos: AtomicU64,
    /// Set by `audioMasterIOChanged`: the plugin's layout is no longer the one activated. The unit
    /// checks it before every plugin call and makes no further call until the owner restarted it.
    halted: AtomicBool,
    restart: AtomicBool,
    relist: AtomicBool,
    need_idle: AtomicBool,
    /// `audioMasterSizeWindow`'s width and height in one word (`0` = none), so the owner never
    /// reads one request's width with another's height.
    resize: AtomicU64,
    automation: OnceLock<AutomationLatch>,
    /// The Win32 id of the instance's owner thread (`0`: none yet, as in the scan's child).
    owner_thread: AtomicU32,
    /// The open editor's host window (`0`: no editor is open). Written by the owner thread only.
    editor_window: AtomicIsize,
    /// The owner thread is inside `audioMasterIdle`'s pump: a nested request is not served.
    idling: AtomicBool,
}

impl HostContext {
    /// A context for one instance, holding the rate and block size its plugin will be told.
    pub(crate) fn new(sample_rate: f64, block_size: i32) -> Result<Arc<Self>, String> {
        let ctx = Self {
            sample_rate: AtomicU64::new(0),
            block_size: AtomicI32::new(0),
            sample_pos: AtomicU64::new(0),
            halted: AtomicBool::new(false),
            restart: AtomicBool::new(false),
            relist: AtomicBool::new(false),
            need_idle: AtomicBool::new(false),
            resize: AtomicU64::new(0),
            automation: OnceLock::new(),
            owner_thread: AtomicU32::new(0),
            editor_window: AtomicIsize::new(0),
            idling: AtomicBool::new(false),
        };
        ctx.set_rate(sample_rate, block_size)?;
        Ok(Arc::new(ctx))
    }

    /// Owner thread, with the plugin stopped: the rate and block size of the next activation.
    pub(crate) fn set_rate(&self, sample_rate: f64, block_size: i32) -> Result<(), String> {
        if !(sample_rate.is_finite() && sample_rate > 0.0) || block_size <= 0 {
            return Err(format!("unusable sample rate {sample_rate} or block size {block_size}"));
        }
        self.sample_rate.store(sample_rate.to_bits(), Relaxed);
        self.block_size.store(block_size, Relaxed);
        Ok(())
    }

    pub(crate) fn sample_rate(&self) -> f64 {
        f64::from_bits(self.sample_rate.load(Relaxed))
    }

    pub(crate) fn block_size(&self) -> i32 {
        self.block_size.load(Relaxed)
    }

    /// The unit, after each block: the frames processed so far.
    pub(crate) fn set_sample_pos(&self, frames: u64) {
        self.sample_pos.store(frames, Relaxed);
    }

    pub(crate) fn sample_pos(&self) -> u64 {
        self.sample_pos.load(Relaxed)
    }

    /// Whether the plugin declared its layout changed (`audioMasterIOChanged`) and no restart has
    /// answered it yet. While it is up, the unit makes no plugin call.
    pub(crate) fn halted(&self) -> bool {
        self.halted.load(Acquire)
    }

    /// Owner thread, once the plugin runs on the layout it now reports.
    pub(crate) fn clear_halted(&self) {
        self.halted.store(false, Release);
    }

    /// Whether the plugin asked for a restart (`audioMasterIOChanged`) since the last take.
    pub(crate) fn take_restart(&self) -> bool {
        self.restart.swap(false, AcqRel)
    }

    /// Whether the plugin's parameters or programs changed behind the host's back
    /// (`audioMasterUpdateDisplay`) since the last take: list them again.
    pub(crate) fn take_relist(&self) -> bool {
        self.relist.swap(false, AcqRel)
    }

    /// Whether the plugin asked for `effIdle` calls (`audioMasterNeedIdle`) since the last take.
    pub(crate) fn take_need_idle(&self) -> bool {
        self.need_idle.swap(false, AcqRel)
    }

    /// The editor size the plugin last asked for (`audioMasterSizeWindow`), width then height.
    pub(crate) fn take_resize(&self) -> Option<(i32, i32)> {
        match self.resize.swap(0, AcqRel) {
            0 => None,
            word => Some(((word >> 32) as i32, word as u32 as i32)),
        }
    }

    /// The calling thread becomes the instance's owner: the one thread on which
    /// `audioMasterIdle` pumps messages and `audioMasterSizeWindow` resizes the editor's window.
    pub(crate) fn bind_owner(&self) {
        // SAFETY: a plain query of the calling thread's id.
        self.owner_thread.store(unsafe { GetCurrentThreadId() }, Release);
    }

    /// Owner thread: the host window the open editor is embedded in (`0` once it closes).
    pub(crate) fn set_editor_window(&self, hwnd: isize) {
        self.editor_window.store(hwnd, Release);
    }

    /// Whether the caller is the owner thread. Never true before `bind_owner`: a thread id is not 0.
    fn on_owner_thread(&self) -> bool {
        // SAFETY: a plain query of the calling thread's id; it allocates, locks and waits on nothing.
        self.owner_thread.load(Acquire) == unsafe { GetCurrentThreadId() }
    }

    /// The automation the plugin reported; `None` until `open_effect` accepted the effect.
    pub(crate) fn automation(&self) -> Option<&AutomationLatch> {
        self.automation.get()
    }

    /// The time a thread that is not processing is told: the defaults at this context's rate and
    /// the unit's last position.
    fn time_snapshot(&self) -> VstTimeInfo {
        time_info(self.sample_rate(), self.sample_pos() as f64)
    }
}

/// A time info with this host's defaults: a positive sample rate, 120 BPM in 4/4, and no validity
/// flag set, since the engine passes no transport to a plugin of any format.
pub(crate) fn time_info(sample_rate: f64, sample_pos: f64) -> VstTimeInfo {
    VstTimeInfo {
        sample_pos,
        sample_rate: if sample_rate.is_finite() && sample_rate > 0.0 { sample_rate } else { FALLBACK_RATE },
        ..TIME_DEFAULTS
    }
}

const TIME_DEFAULTS: VstTimeInfo = VstTimeInfo {
    sample_pos: 0.0,
    sample_rate: FALLBACK_RATE,
    nano_seconds: 0.0,
    ppq_pos: 0.0,
    tempo: 120.0,
    bar_start_pos: 0.0,
    cycle_start_pos: 0.0,
    cycle_end_pos: 0.0,
    time_sig_numerator: 4,
    time_sig_denominator: 4,
    smpte_offset: 0,
    smpte_frame_rate: 0,
    samples_to_next_clock: 0,
    flags: 0,
};

// Const-initialised and without destructors: reading one never allocates or registers anything, on
// the audio thread or on a thread the plugin made.
thread_local! {
    /// The context of the instance whose entry is running on this thread (`Creating`).
    static CREATING: Cell<*const HostContext> = const { Cell::new(std::ptr::null()) };
    /// The unit processing on this thread and its time info (`ProcessingScope`).
    static PROCESSING: Cell<(*const HostContext, *mut VstTimeInfo)> =
        const { Cell::new((std::ptr::null(), std::ptr::null_mut())) };
    /// What `audioMasterGetTime` hands this thread while it is not the one processing.
    static TIME_SNAPSHOT: UnsafeCell<VstTimeInfo> = const { UnsafeCell::new(TIME_DEFAULTS) };
}

/// Marks this thread as running the entry of `ctx`'s instance: a constructor may call the host with
/// no effect, or with an effect that does not point at its context yet. It nests (an entry that
/// creates another instance) and restores the outer creation when it drops, on an unwind too.
struct Creating {
    outer: *const HostContext,
}

impl Creating {
    fn enter(ctx: &HostContext) -> Self {
        Self { outer: CREATING.replace(ctx) }
    }
}

impl Drop for Creating {
    fn drop(&mut self) {
        CREATING.set(self.outer);
    }
}

/// Marks this thread as inside a unit's plugin call, on the audio path: while it lives, the plugin
/// reads the unit's own time info and is told it runs at realtime level. A unit holds one around
/// every call it makes into its plugin; it restores the outer scope when it drops.
pub(crate) struct ProcessingScope {
    outer: (*const HostContext, *mut VstTimeInfo),
}

impl ProcessingScope {
    /// # Safety
    /// `time` must stay valid, and be written by no other thread, until the scope drops.
    pub(crate) unsafe fn enter(ctx: &HostContext, time: *mut VstTimeInfo) -> Self {
        Self { outer: PROCESSING.replace((ctx, time)) }
    }
}

impl Drop for ProcessingScope {
    fn drop(&mut self) {
        PROCESSING.set(self.outer);
    }
}

/// The context a callback belongs to: the one its effect points at, else the one being created on
/// this thread.
///
/// # Safety
/// `effect` is null or an `AEffect` a plugin of this host passed.
unsafe fn context_of<'a>(effect: *mut AEffect) -> Option<&'a HostContext> {
    let mut ctx = std::ptr::null();
    if !effect.is_null() {
        // SAFETY: the field is read as the atomic `open_effect` writes it as, so a callback on
        // another thread never races that write.
        ctx = unsafe { AtomicIsize::from_ptr(&raw mut (*effect).resvd1) }.load(Acquire) as *const HostContext;
    }
    if ctx.is_null() {
        ctx = CREATING.get();
    }
    // SAFETY: a context outlives its plugin (`HostContext`), and a creation's outlives its guard.
    unsafe { ctx.as_ref() }
}

/// Whether the NUL-terminated string at `text` is exactly `name`. Reads no byte past the first one
/// that differs, the plugin's terminator included.
///
/// # Safety
/// `text` is a NUL-terminated string.
unsafe fn c_str_is(text: *const u8, name: &[u8]) -> bool {
    for (i, &byte) in name.iter().enumerate() {
        // SAFETY: every byte before this one matched a non-NUL byte, so this one is in the string.
        if unsafe { *text.add(i) } != byte {
            return false;
        }
    }
    // SAFETY: as above; this is the terminator or a longer string's next character.
    unsafe { *text.add(name.len()) == 0 }
}

/// `audioMasterIdle`: the plugin asks the host to run its idle work now (a modal loop of its own
/// is waiting on it). Called on the owner thread, outside a unit's plugin call, it runs one bounded
/// batch of that thread's Win32 messages and answers 1; a request from inside that batch is not
/// served again, and no owner request is serviced from here. From any other thread, from the
/// processing thread (the owner's own included, while it renders an idle engine's block), and
/// without an owner, the answer is 0 and nothing happens.
fn owner_idle(ctx: &HostContext, in_process: bool) -> isize {
    if in_process || !ctx.on_owner_thread() || ctx.idling.swap(true, AcqRel) {
        return 0;
    }
    pump_thread_messages();
    ctx.idling.store(false, Release);
    1
}

/// `audioMasterSizeWindow`: the plugin's editor wants a new size. Called on the owner thread,
/// outside a unit's plugin call, with an editor open, the host window is resized here and now and
/// the answer is 1 only when the client area became exactly what was asked (Windows clamps a window
/// to the screen; one it clamped goes back to the size it had, so a refusal changes nothing); a size
/// queued earlier is dropped. From any other thread, and from the
/// processing thread, the size is latched for the owner (`HostContext::take_resize`) and the answer
/// is 0, "not resized yet": no window function is called there.
fn owner_size_window(ctx: &HostContext, width: i32, height: isize, in_process: bool) -> isize {
    let (Ok(width @ 1..), Ok(height @ 1..)) = (u32::try_from(width), i32::try_from(height)) else {
        return 0;
    };
    let window = ctx.editor_window.load(Acquire);
    if in_process || window == 0 || !ctx.on_owner_thread() {
        ctx.resize.store(u64::from(width) << 32 | u64::from(height as u32), Release);
        return 0;
    }
    ctx.resize.store(0, Release);
    let window = HWND(window as *mut c_void);
    let before = client_size(window);
    if set_client_size(window, width, height as u32) == Some((width, height as u32)) {
        return 1;
    }
    let _ = set_client_size(window, before.0, before.1);
    0
}

/// The host callback every instance is created with. A plugin calls it from any thread, the audio
/// thread included: it never allocates, logs, locks or waits. Without a context (no effect that
/// points at one, no creation on this thread) it still answers its version, and 0 to the rest.
///
/// # Safety
/// As a plugin calls it: `effect` is null or its `AEffect`, and `ptr` is what `opcode` defines.
pub(crate) unsafe extern "C" fn host_callback(
    effect: *mut AEffect,
    opcode: i32,
    index: i32,
    value: isize,
    ptr: *mut c_void,
    opt: f32,
) -> isize {
    // SAFETY: the caller's contract.
    let Some(ctx) = (unsafe { context_of(effect) }) else {
        return if opcode == AUDIO_MASTER_VERSION { VST_VERSION_2_4 } else { 0 };
    };
    let (processing, unit_time) = PROCESSING.get();
    let in_process = std::ptr::eq(processing, ctx) && !unit_time.is_null();
    match opcode {
        AUDIO_MASTER_VERSION => VST_VERSION_2_4,
        AUDIO_MASTER_CURRENT_ID => 0,
        AUDIO_MASTER_AUTOMATE => {
            if let Some(latch) = ctx.automation.get() {
                latch.record(index, opt);
            }
            0
        }
        AUDIO_MASTER_IDLE => owner_idle(ctx, in_process),
        AUDIO_MASTER_WANT_MIDI => 1,
        // The processing thread reads the time info its unit writes; every other thread gets its
        // own snapshot, so no thread reads what another writes.
        AUDIO_MASTER_GET_TIME if in_process => unit_time as isize,
        AUDIO_MASTER_GET_TIME => TIME_SNAPSHOT.with(|snapshot| {
            // SAFETY: only this thread ever touches its snapshot, and holds no reference to it.
            unsafe { *snapshot.get() = ctx.time_snapshot() };
            snapshot.get() as isize
        }),
        AUDIO_MASTER_IO_CHANGED => {
            ctx.halted.store(true, Release);
            ctx.restart.store(true, Release);
            1
        }
        AUDIO_MASTER_NEED_IDLE => {
            ctx.need_idle.store(true, Release);
            1
        }
        AUDIO_MASTER_SIZE_WINDOW => owner_size_window(ctx, index, value, in_process),
        AUDIO_MASTER_GET_SAMPLE_RATE => ctx.sample_rate().round() as isize,
        AUDIO_MASTER_GET_BLOCK_SIZE => ctx.block_size() as isize,
        AUDIO_MASTER_GET_CURRENT_PROCESS_LEVEL if in_process => PROCESS_LEVEL_REALTIME,
        AUDIO_MASTER_GET_CURRENT_PROCESS_LEVEL => PROCESS_LEVEL_USER,
        AUDIO_MASTER_GET_VENDOR_STRING | AUDIO_MASTER_GET_PRODUCT_STRING => {
            if ptr.is_null() {
                return 0;
            }
            // SAFETY: the plugin's buffer for either string holds at least 64 bytes (`HOST_NAME`).
            unsafe { std::ptr::copy_nonoverlapping(HOST_NAME.as_ptr(), ptr.cast::<u8>(), HOST_NAME.len()) };
            1
        }
        AUDIO_MASTER_GET_VENDOR_VERSION => HOST_VERSION,
        AUDIO_MASTER_CAN_DO => {
            // SAFETY: `ptr` is the NUL-terminated feature name when it is not null.
            let known = !ptr.is_null() && HOST_CAN_DO.iter().any(|name| unsafe { c_str_is(ptr.cast(), name) });
            isize::from(known)
        }
        AUDIO_MASTER_UPDATE_DISPLAY => {
            ctx.relist.store(true, Release);
            1
        }
        AUDIO_MASTER_BEGIN_EDIT | AUDIO_MASTER_END_EDIT => 1,
        _ => 0,
    }
}

/// Why `open_effect` produced no effect.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OpenError {
    /// The entry made no instance: the module can be unloaded.
    NoInstance(String),
    /// The entry made an instance this host will not run. Nothing of it was called, so it was never
    /// closed either: its module must stay loaded (`Vst2Module::leak`), with its context.
    Refused(String),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::NoInstance(e) | OpenError::Refused(e) => f.write_str(e),
        }
    }
}

/// What an accepted effect declared, checked: every count here is safe to size a buffer from, and
/// every function pointer the host will call is there.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EffectInfo {
    pub(crate) inputs: usize,
    pub(crate) outputs: usize,
    pub(crate) params: usize,
    pub(crate) programs: usize,
    /// The plugin's latency in frames.
    pub(crate) initial_delay: u32,
    pub(crate) flags: i32,
    pub(crate) unique_id: i32,
    pub(crate) dispatcher: DispatcherFn,
    /// The call that renders a block: `processReplacing` when the plugin has it (`replacing`), else
    /// the accumulating `process` of a VST 1.0-era plugin, which ADDS into its outputs: the caller
    /// clears them first.
    pub(crate) process: ProcessFn,
    pub(crate) replacing: bool,
    /// Both are there whenever `params` is not 0.
    pub(crate) set_parameter: Option<SetParameterFn>,
    pub(crate) get_parameter: Option<GetParameterFn>,
}

impl EffectInfo {
    pub(crate) fn is_synth(&self) -> bool {
        self.flags & EFF_FLAGS_IS_SYNTH != 0
    }

    pub(crate) fn has_editor(&self) -> bool {
        self.flags & EFF_FLAGS_HAS_EDITOR != 0
    }

    /// Whether the plugin's state is an opaque chunk (`effGetChunk`) rather than its parameters.
    pub(crate) fn program_chunks(&self) -> bool {
        self.flags & EFF_FLAGS_PROGRAM_CHUNKS != 0
    }
}

/// Check what `effect` declares before anything is sized from it or called through it. A restart
/// runs it again: a plugin may change its counts with `audioMasterIOChanged`.
///
/// # Safety
/// `effect` points at memory readable as an `AEffect`.
pub(crate) unsafe fn validate(effect: *const AEffect) -> Result<EffectInfo, String> {
    // SAFETY: the caller's contract; every field is valid for any bit pattern (a null function
    // pointer reads as `None`).
    let e = unsafe { &*effect };
    if e.magic != EFFECT_MAGIC {
        return Err(format!("not a VST2 effect (magic {:#010x})", e.magic));
    }
    let dispatcher = e.dispatcher.ok_or("the effect has no dispatcher")?;
    let replacing = e.flags & EFF_FLAGS_CAN_REPLACING != 0;
    let process = if replacing {
        e.process_replacing.ok_or("the effect declares processReplacing and has none")?
    } else {
        e.process.ok_or("the effect has no process function")?
    };
    let inputs = checked_plugin_channels(i64::from(e.num_inputs), "VST2 inputs", true)? as usize;
    let outputs = checked_plugin_channels(i64::from(e.num_outputs), "VST2 outputs", false)? as usize;
    let params = checked_plugin_params(i64::from(e.num_params), "VST2 effect")?;
    let programs = usize::try_from(e.num_programs).map_err(|_| format!("VST2 effect reports {} programs", e.num_programs))?;
    if !(0..=MAX_INITIAL_DELAY).contains(&e.initial_delay) {
        return Err(format!("VST2 effect reports unsupported latency {} frames", e.initial_delay));
    }
    if params > 0 && (e.set_parameter.is_none() || e.get_parameter.is_none()) {
        return Err(format!("the effect declares {params} parameters and lacks setParameter or getParameter"));
    }
    Ok(EffectInfo {
        inputs,
        outputs,
        params,
        programs,
        initial_delay: e.initial_delay as u32,
        flags: e.flags,
        unique_id: e.unique_id,
        dispatcher,
        process,
        replacing,
        set_parameter: e.set_parameter,
        get_parameter: e.get_parameter,
    })
}

/// One open VST2 instance, as `open_effect` accepted it. Not `Send`: it stays on the thread that
/// opened it. Nothing closes it on drop: `close` it, in order, before its module unloads.
pub(crate) struct Vst2Effect {
    raw: NonNull<AEffect>,
    info: EffectInfo,
}

impl Vst2Effect {
    /// The plugin's own structure, for the calls that are not the dispatcher's.
    pub(crate) fn raw(&self) -> *mut AEffect {
        self.raw.as_ptr()
    }

    /// What the effect declared when it was opened.
    pub(crate) fn info(&self) -> &EffectInfo {
        &self.info
    }

    /// Call the plugin's dispatcher. What the return means is the opcode's own: for `effOpen`,
    /// `effMainsChanged`, `effSetChunk`, `effStartProcess` and `effStopProcess`, 0 is NOT a failure.
    ///
    /// # Safety
    /// On the thread that opened the effect, with `ptr` being what `opcode` defines.
    pub(crate) unsafe fn dispatch(&self, opcode: i32, index: i32, value: isize, ptr: *mut c_void, opt: f32) -> isize {
        // SAFETY: the dispatcher `validate` found, called as the caller's contract allows.
        unsafe { (self.info.dispatcher)(self.raw.as_ptr(), opcode, index, value, ptr, opt) }
    }

    /// A string the plugin writes for `opcode` (a name, a label, a parameter's display); `None`
    /// when it wrote nothing. Plugins overrun the short lengths the format names, so the buffer is
    /// far larger than any of them.
    ///
    /// # Safety
    /// On the thread that opened the effect, with an opcode that takes a string buffer in `ptr`.
    pub(crate) unsafe fn string(&self, opcode: i32, index: i32) -> Option<String> {
        let mut buffer = [0u8; 256];
        // SAFETY: the caller's contract; the buffer outlives the call.
        unsafe { self.dispatch(opcode, index, 0, buffer.as_mut_ptr().cast(), 0.0) };
        let end = buffer[..255].iter().position(|&b| b == 0).unwrap_or(255);
        let text = String::from_utf8_lossy(&buffer[..end]).trim().to_string();
        (!text.is_empty()).then_some(text)
    }

    /// The plugin's own name: `effGetEffectName`, else `effGetProductString`; `None` when it gives
    /// neither (the caller falls back to the file's stem).
    ///
    /// # Safety
    /// On the thread that opened the effect.
    pub(crate) unsafe fn name(&self) -> Option<String> {
        // SAFETY: the caller's contract; both opcodes take a string buffer.
        unsafe { self.string(EFF_GET_EFFECT_NAME, 0).or_else(|| self.string(EFF_GET_PRODUCT_STRING, 0)) }
    }

    /// `effClose`: the plugin frees its instance. Its context and its module must still be there.
    ///
    /// # Safety
    /// On the thread that opened the effect, with no other thread inside a call to it.
    pub(crate) unsafe fn close(self) {
        // SAFETY: the caller's contract; the effect is consumed, so nothing calls it again.
        unsafe { self.dispatch(EFF_CLOSE, 0, 0, std::ptr::null_mut(), 0.0) };
    }
}

/// Create one instance through `entry` (`Vst2Module::entry`) for `ctx`, which already holds the
/// rate and block size the plugin may ask for from inside its constructor, and open it: the entry
/// call under the creation guard, `validate`, the effect bound to its context (`resvd1`), `effOpen`.
/// `ctx` takes one instance in its life.
///
/// # Safety
/// `entry` is a VST2 entry whose module stays loaded; the caller keeps `ctx` alive for as long as
/// the plugin may call the host (`HostContext`).
pub(crate) unsafe fn open_effect(entry: EntryFn, ctx: &Arc<HostContext>) -> Result<Vst2Effect, OpenError> {
    if ctx.automation.get().is_some() {
        return Err(OpenError::NoInstance("this host context already has its effect".to_string()));
    }
    let raw = {
        let _creating = Creating::enter(ctx);
        // SAFETY: the caller's contract: this runs the plugin's constructor.
        unsafe { entry(host_callback) }
    };
    let raw = NonNull::new(raw).ok_or_else(|| OpenError::NoInstance("the plugin's entry returned no effect".to_string()))?;
    // SAFETY: a non-null entry result is the plugin's `AEffect`.
    let info = unsafe { validate(raw.as_ptr()) }.map_err(OpenError::Refused)?;
    // Sized from the checked count; a callback that arrived before this found no latch and was dropped.
    let _ = ctx.automation.set(AutomationLatch::new(info.params));
    // SAFETY: the field is written as the atomic the callback reads it as (`context_of`).
    unsafe { AtomicIsize::from_ptr(&raw mut (*raw.as_ptr()).resvd1) }.store(Arc::as_ptr(ctx) as isize, Release);
    let effect = Vst2Effect { raw, info };
    // SAFETY: the opening thread, an opcode without a pointer. Its return carries no verdict.
    unsafe { effect.dispatch(EFF_OPEN, 0, 0, std::ptr::null_mut(), 0.0) };
    Ok(effect)
}

#[cfg(test)]
#[path = "vst2_fixture.rs"]
pub(crate) mod fixture;

#[cfg(test)]
mod tests {
    use super::fixture::{self, Shape};
    use super::*;
    use std::sync::Barrier;

    const NULL: *mut c_void = std::ptr::null_mut();

    /// Open the fixture in `shape` at `rate`; the context comes back with the effect.
    fn open(shape: Shape, rate: f64) -> (Arc<HostContext>, Result<Vst2Effect, OpenError>) {
        let ctx = HostContext::new(rate, 256).unwrap();
        // SAFETY: the fixture's entry is in this process and `ctx` is returned to the caller.
        let opened = fixture::with_shape(shape, || unsafe { open_effect(fixture::entry, &ctx) });
        (ctx, opened)
    }

    fn call(effect: &Vst2Effect, opcode: i32, index: i32, value: isize, opt: f32) -> isize {
        // SAFETY: the effect is open and none of the opcodes used this way reads `ptr`.
        unsafe { host_callback(effect.raw(), opcode, index, value, NULL, opt) }
    }

    #[test]
    fn a_constructor_reaches_its_context_with_no_effect_and_with_an_unbound_one() {
        let (ctx, opened) = open(Shape::default(), 88_200.0);
        let effect = opened.unwrap();
        // The entry asked for the rate with a null effect, then with its effect before `resvd1` was
        // set, and for the block size the same two ways.
        assert_eq!(fixture::take_seen(), vec![88_200, 88_200, 256, 256]);
        // SAFETY: the open effect's own structure.
        assert_eq!(unsafe { (*effect.raw()).resvd1 }, Arc::as_ptr(&ctx) as isize);
        assert_eq!(
            fixture::take_calls(),
            vec![(EFF_OPEN, true)],
            "effOpen is the first call, with the effect bound to its context"
        );
        // Outside a creation, a callback with no effect has no context.
        // SAFETY: a null effect and an opcode that reads no pointer.
        assert_eq!(unsafe { host_callback(std::ptr::null_mut(), AUDIO_MASTER_GET_SAMPLE_RATE, 0, 0, NULL, 0.0) }, 0);
        // SAFETY: the opening thread.
        unsafe { effect.close() };
        assert_eq!(fixture::take_calls(), vec![(EFF_CLOSE, true)]);
    }

    #[test]
    fn a_creation_inside_a_creation_keeps_both_contexts_apart() {
        fn inner() {
            let (_ctx, opened) = open(Shape::default(), 96_000.0);
            // SAFETY: the opening thread.
            unsafe { opened.unwrap().close() };
        }
        let (_ctx, opened) = open(Shape { during_entry: Some(inner), ..Shape::default() }, 44_100.0);
        // Outer rate (null effect), the whole inner creation, then the outer rate again.
        assert_eq!(fixture::take_seen(), vec![44_100, 96_000, 96_000, 256, 256, 44_100, 256, 256]);
        // SAFETY: the opening thread.
        unsafe { opened.unwrap().close() };
    }

    #[test]
    fn two_creations_at_once_on_two_threads_keep_their_contexts_apart() {
        static BOTH_INSIDE: Barrier = Barrier::new(2);
        fn meet() {
            BOTH_INSIDE.wait();
        }
        let threads: Vec<_> = [32_000.0, 192_000.0]
            .into_iter()
            .map(|rate: f64| {
                std::thread::spawn(move || {
                    let (_ctx, opened) = open(Shape { during_entry: Some(meet), ..Shape::default() }, rate);
                    // SAFETY: the opening thread.
                    unsafe { opened.unwrap().close() };
                    (rate as isize, fixture::take_seen())
                })
            })
            .collect();
        for thread in threads {
            let (rate, seen) = thread.join().unwrap();
            assert_eq!(seen, vec![rate, rate, 256, 256], "each entry saw its own context on both sides of the wait");
        }
    }

    #[test]
    fn an_effect_this_host_cannot_run_is_refused_and_never_called() {
        let refused: Vec<(&str, Shape)> = vec![
            ("magic", Shape { magic: 0x1234_5678, ..Shape::default() }),
            ("dispatcher", Shape { dispatcher: false, ..Shape::default() }),
            ("processReplacing", Shape { process_replacing: false, ..Shape::default() }),
            ("process function", Shape { flags: 0, process: false, ..Shape::default() }),
            ("channel count -1", Shape { inputs: -1, ..Shape::default() }),
            ("channel count 65", Shape { inputs: 65, ..Shape::default() }),
            ("channel count 0", Shape { outputs: 0, ..Shape::default() }),
            ("channel count 65", Shape { outputs: 65, ..Shape::default() }),
            ("parameter count -1", Shape { params: -1, ..Shape::default() }),
            ("parameter count 65537", Shape { params: 65_537, ..Shape::default() }),
            ("-1 programs", Shape { programs: -1, ..Shape::default() }),
            ("latency -1", Shape { delay: -1, ..Shape::default() }),
            ("latency 16777217", Shape { delay: MAX_INITIAL_DELAY + 1, ..Shape::default() }),
            ("setParameter", Shape { set_parameter: false, ..Shape::default() }),
            ("getParameter", Shape { get_parameter: false, ..Shape::default() }),
        ];
        for (what, shape) in refused {
            let (ctx, opened) = open(shape, 48_000.0);
            match opened {
                Err(OpenError::Refused(e)) => assert!(e.contains(what), "{what}: {e}"),
                other => panic!("{what}: expected a refusal, got {:?}", other.map(|_| "an effect")),
            }
            assert_eq!(fixture::take_calls(), vec![], "{what}: a refused effect is never called");
            assert!(ctx.automation().is_none(), "{what}: nothing is sized from a refused effect");
            fixture::take_seen();
        }
        let (_ctx, opened) = open(Shape { null: true, ..Shape::default() }, 48_000.0);
        assert!(matches!(opened, Err(OpenError::NoInstance(_))), "an entry that returns null made no instance");
    }

    #[test]
    fn the_edges_of_what_is_accepted() {
        // A VST 1.0-era plugin: no processReplacing, the accumulating call instead.
        let old = Shape { flags: 0, process_replacing: false, ..Shape::default() };
        let (_ctx, opened) = open(old, 48_000.0);
        let effect = opened.unwrap();
        assert!(!effect.info().replacing);
        // SAFETY: the opening thread.
        unsafe { effect.close() };
        // No parameters: neither parameter function is needed. No input: a synth.
        let bare = Shape {
            params: 0,
            set_parameter: false,
            get_parameter: false,
            inputs: 0,
            outputs: 64,
            programs: 0,
            delay: MAX_INITIAL_DELAY,
            flags: EFF_FLAGS_CAN_REPLACING | EFF_FLAGS_IS_SYNTH,
            ..Shape::default()
        };
        let (ctx, opened) = open(bare, 48_000.0);
        let effect = opened.unwrap();
        let info = *effect.info();
        assert!(info.replacing && info.is_synth() && !info.has_editor() && !info.program_chunks());
        assert_eq!((info.inputs, info.outputs, info.params, info.programs), (0, 64, 0, 0));
        assert_eq!(info.initial_delay, MAX_INITIAL_DELAY as u32);
        // A second effect on one context is refused before any entry runs.
        // SAFETY: the fixture's entry; it is never reached.
        let again = fixture::with_shape(Shape::default(), || unsafe { open_effect(fixture::entry, &ctx) });
        assert!(matches!(again, Err(OpenError::NoInstance(_))));
        // SAFETY: the opening thread.
        unsafe { effect.close() };
        fixture::take_seen();
        fixture::take_calls();
    }

    #[test]
    fn automation_from_several_threads_loses_no_parameters_last_value() {
        const THREADS: usize = 6;
        const PARAMS: usize = 200; // Four bitmap words, each shared by every thread.
        const ROUNDS: usize = 2_000;
        let (ctx, opened) = open(Shape { params: PARAMS as i32, ..Shape::default() }, 48_000.0);
        let effect = opened.unwrap();
        let raw = effect.raw() as usize;
        let last = |thread: usize, param: usize| (thread * 1_000_000 + param * 1_000 + ROUNDS - 1) as f32;
        let mut seen = vec![f32::NAN; PARAMS];
        std::thread::scope(|scope| {
            // Thread t owns the parameters p with p % THREADS == t, so each has one last value.
            let producers: Vec<_> = (0..THREADS)
                .map(|thread| {
                    scope.spawn(move || {
                        for round in 0..ROUNDS {
                            for param in (thread..PARAMS).step_by(THREADS) {
                                let value = (thread * 1_000_000 + param * 1_000 + round) as f32;
                                // SAFETY: the effect stays open for the scope; automate reads no pointer.
                                unsafe { host_callback(raw as *mut AEffect, AUDIO_MASTER_AUTOMATE, param as i32, 0, NULL, value) };
                            }
                        }
                    })
                })
                .collect();
            // The owner drains while they report.
            let latch = ctx.automation().unwrap();
            while producers.iter().any(|p| !p.is_finished()) {
                latch.drain(|index, value| seen[index] = value);
            }
        });
        ctx.automation().unwrap().drain(|index, value| seen[index] = value);
        for (param, value) in seen.iter().enumerate() {
            assert_eq!(*value, last(param % THREADS, param), "parameter {param}");
        }
        // Nothing is pending, and an index the plugin never declared is ignored.
        let mut pending = 0;
        ctx.automation().unwrap().drain(|_, _| pending += 1);
        assert_eq!(call(&effect, AUDIO_MASTER_AUTOMATE, PARAMS as i32, 0, 1.0), 0);
        assert_eq!(call(&effect, AUDIO_MASTER_AUTOMATE, -1, 0, 1.0), 0);
        assert_eq!(call(&effect, AUDIO_MASTER_AUTOMATE, i32::MAX, 0, 1.0), 0);
        ctx.automation().unwrap().drain(|_, _| pending += 1);
        assert_eq!(pending, 0);
        // The plugin was never called for any of it: no echo, no getParameter.
        assert_eq!(fixture::take_calls(), vec![(EFF_OPEN, true)]);
        assert_eq!(fixture::parameter_calls(), 0);
        // SAFETY: the opening thread; the producers have joined.
        unsafe { effect.close() };
    }

    /// Every producer reports EVERY parameter while the owner drains, so each value and each bitmap
    /// word is fought over. Whatever the drain hands over is a value some thread reported for that
    /// parameter, and once all of them have stopped, one last report per parameter (still against
    /// the running drain) is what the owner ends up holding: a later report is never lost to an
    /// earlier one's delivery.
    #[test]
    fn automation_shared_between_threads_ends_on_the_value_reported_last() {
        const THREADS: usize = 6;
        const PARAMS: usize = 130; // Three bitmap words, the last one partly used.
        const ROUNDS: usize = 2_000;
        // A value names its parameter and who reported it; 7 is no thread: the last report.
        let value_of = |param: usize, reporter: usize| (param * 8 + reporter) as f32;
        let (ctx, opened) = open(Shape { params: PARAMS as i32, ..Shape::default() }, 48_000.0);
        let effect = opened.unwrap();
        let raw = effect.raw() as usize;
        let report = move |param: usize, reporter: usize| {
            // SAFETY: the effect stays open for the scope below; automate reads no pointer.
            unsafe { host_callback(raw as *mut AEffect, AUDIO_MASTER_AUTOMATE, param as i32, 0, NULL, value_of(param, reporter)) };
        };
        let all_reported = Barrier::new(THREADS);
        let mut seen = vec![f32::NAN; PARAMS];
        let mut deliveries = 0usize;
        std::thread::scope(|scope| {
            let all_reported = &all_reported;
            let producers: Vec<_> = (0..THREADS)
                .map(|thread| {
                    scope.spawn(move || {
                        for _ in 0..ROUNDS {
                            for param in 0..PARAMS {
                                report(param, thread);
                            }
                        }
                        // Every shared report is in: each parameter now gets its last one, from
                        // the thread it falls to.
                        all_reported.wait();
                        for param in (thread..PARAMS).step_by(THREADS) {
                            report(param, 7);
                        }
                    })
                })
                .collect();
            let latch = ctx.automation().unwrap();
            let drain = |seen: &mut Vec<f32>, deliveries: &mut usize| {
                latch.drain(|index, value| {
                    let (param, reporter) = (value as usize / 8, value as usize % 8);
                    assert!(param == index && (reporter < THREADS || reporter == 7), "parameter {index} was handed {value}");
                    seen[index] = value;
                    *deliveries += 1;
                })
            };
            while producers.iter().any(|p| !p.is_finished()) {
                drain(&mut seen, &mut deliveries);
            }
            drain(&mut seen, &mut deliveries);
        });
        for (param, value) in seen.iter().enumerate() {
            assert_eq!(*value, value_of(param, 7), "parameter {param} ends on its last report");
        }
        assert!(deliveries >= PARAMS);
        let mut pending = 0;
        ctx.automation().unwrap().drain(|_, _| pending += 1);
        assert_eq!(pending, 0, "nothing is delivered twice");
        // SAFETY: the opening thread; the producers have joined.
        unsafe { effect.close() };
        fixture::take_seen();
        fixture::take_calls();
    }

    #[test]
    fn the_processing_thread_and_every_other_thread_get_their_own_time_info() {
        let (ctx, opened) = open(Shape::default(), 48_000.0);
        let effect = opened.unwrap();
        ctx.set_sample_pos(4_800);
        // Not processing: this thread's snapshot, at the defaults.
        let snapshot = call(&effect, AUDIO_MASTER_GET_TIME, 0, 0, 0.0) as *const VstTimeInfo;
        // SAFETY: the callback's answer points at this thread's snapshot.
        let told = unsafe { *snapshot };
        assert_eq!(told, VstTimeInfo { sample_pos: 4_800.0, sample_rate: 48_000.0, ..TIME_DEFAULTS });
        assert_eq!((told.tempo, told.time_sig_numerator, told.time_sig_denominator, told.flags), (120.0, 4, 4, 0));
        assert_eq!(call(&effect, AUDIO_MASTER_GET_CURRENT_PROCESS_LEVEL, 0, 0, 0.0), PROCESS_LEVEL_USER);

        // Processing: the unit's own time info, and realtime level.
        let mut unit_time = time_info(48_000.0, 9_600.0);
        let unit_ptr: *mut VstTimeInfo = &mut unit_time;
        {
            // SAFETY: `unit_time` outlives the scope and no other thread touches it.
            let _scope = unsafe { ProcessingScope::enter(&ctx, unit_ptr) };
            assert_eq!(call(&effect, AUDIO_MASTER_GET_TIME, 0, 0, 0.0) as *mut VstTimeInfo, unit_ptr);
            assert_eq!(call(&effect, AUDIO_MASTER_GET_CURRENT_PROCESS_LEVEL, 0, 0, 0.0), PROCESS_LEVEL_REALTIME);
            // Another thread, meanwhile: its own snapshot, never the unit's struct or this thread's.
            let raw = effect.raw() as usize;
            let other = std::thread::spawn(move || {
                // SAFETY: the effect is open; neither opcode reads `ptr`.
                unsafe {
                    let time = host_callback(raw as *mut AEffect, AUDIO_MASTER_GET_TIME, 0, 0, NULL, 0.0);
                    let level = host_callback(raw as *mut AEffect, AUDIO_MASTER_GET_CURRENT_PROCESS_LEVEL, 0, 0, NULL, 0.0);
                    (time, (*(time as *const VstTimeInfo)).sample_pos, level)
                }
            })
            .join()
            .unwrap();
            assert_ne!(other.0, unit_ptr as isize);
            assert_ne!(other.0, snapshot as isize);
            assert_eq!((other.1, other.2), (4_800.0, PROCESS_LEVEL_USER));
            // Another instance's callback on this thread is not inside ITS unit's processing.
            let (_other_ctx, other_effect) = open(Shape::default(), 44_100.0);
            let other_effect = other_effect.unwrap();
            assert_eq!(call(&other_effect, AUDIO_MASTER_GET_TIME, 0, 0, 0.0), snapshot as isize);
            assert_eq!(call(&other_effect, AUDIO_MASTER_GET_CURRENT_PROCESS_LEVEL, 0, 0, 0.0), PROCESS_LEVEL_USER);
            // SAFETY: the opening thread.
            unsafe { other_effect.close() };
        }
        // The scope is over.
        assert_eq!(call(&effect, AUDIO_MASTER_GET_TIME, 0, 0, 0.0), snapshot as isize);
        assert_eq!(call(&effect, AUDIO_MASTER_GET_CURRENT_PROCESS_LEVEL, 0, 0, 0.0), PROCESS_LEVEL_USER);
        // SAFETY: the opening thread.
        unsafe { effect.close() };
        fixture::take_seen();
        fixture::take_calls();
    }

    #[test]
    fn a_callback_with_no_context_answers_only_its_version() {
        // An effect that points at no context, on a thread that creates nothing.
        // SAFETY: an all-zero `AEffect` is valid (integers, null pointers, `None`s).
        let mut unbound: AEffect = unsafe { std::mem::zeroed() };
        let mut buffer = [0x55u8; 64];
        for effect in [std::ptr::null_mut(), &mut unbound as *mut AEffect] {
            for opcode in -1..=64 {
                // SAFETY: with no context the callback reads no pointer; the buffer is there anyway.
                let answer = unsafe { host_callback(effect, opcode, 0, 0, buffer.as_mut_ptr().cast(), 0.5) };
                let expected = if opcode == AUDIO_MASTER_VERSION { 2400 } else { 0 };
                assert_eq!(answer, expected, "opcode {opcode}");
            }
        }
        assert_eq!(buffer, [0x55u8; 64], "nothing is written without a context");
    }

    #[test]
    fn the_callback_answers_and_latches_for_its_owner() {
        let (ctx, opened) = open(Shape::default(), 44_100.0);
        let effect = opened.unwrap();
        assert_eq!(call(&effect, AUDIO_MASTER_VERSION, 0, 0, 0.0), 2400);
        assert_eq!(call(&effect, AUDIO_MASTER_CURRENT_ID, 0, 0, 0.0), 0);
        assert_eq!(call(&effect, AUDIO_MASTER_GET_SAMPLE_RATE, 0, 0, 0.0), 44_100);
        assert_eq!(call(&effect, AUDIO_MASTER_GET_BLOCK_SIZE, 0, 0, 0.0), 256);
        assert_eq!(call(&effect, AUDIO_MASTER_WANT_MIDI, 0, 0, 0.0), 1);
        assert_eq!(call(&effect, AUDIO_MASTER_BEGIN_EDIT, 0, 0, 0.0), 1);
        assert_eq!(call(&effect, AUDIO_MASTER_END_EDIT, 0, 0, 0.0), 1);
        assert_eq!(call(&effect, AUDIO_MASTER_IDLE, 0, 0, 0.0), 0);
        assert_eq!(call(&effect, 4, 0, 0, 0.0), 0, "an opcode this host does not answer");
        assert_eq!(call(&effect, AUDIO_MASTER_GET_VENDOR_VERSION, 0, 0, 0.0), HOST_VERSION);
        assert!(HOST_VERSION > 0);

        for opcode in [AUDIO_MASTER_GET_VENDOR_STRING, AUDIO_MASTER_GET_PRODUCT_STRING] {
            let mut buffer = [0x55u8; 64];
            // SAFETY: a 64-byte buffer, as a plugin passes.
            assert_eq!(unsafe { host_callback(effect.raw(), opcode, 0, 0, buffer.as_mut_ptr().cast(), 0.0) }, 1);
            assert_eq!(&buffer[..10], b"BleepLoop\0");
            assert!(buffer[10..].iter().all(|&b| b == 0x55), "only the name and its terminator are written");
            assert_eq!(call(&effect, opcode, 0, 0, 0.0), 0, "no buffer, no answer");
        }
        let names: [(&[u8], isize); 8] = [
            (b"sendVstEvents\0", 1),
            (b"sendVstMidiEvent\0", 1),
            (b"sizeWindow\0", 1),
            (b"startStopProcess\0", 1),
            (b"sendVstEventsX\0", 0),
            (b"sendVstEvent\0", 0),
            (b"sendVstTimeInfo\0", 0),
            (b"\0", 0),
        ];
        for (name, known) in names {
            // SAFETY: a NUL-terminated name.
            let answer = unsafe { host_callback(effect.raw(), AUDIO_MASTER_CAN_DO, 0, 0, name.as_ptr() as *mut c_void, 0.0) };
            assert_eq!(answer, known, "{}", String::from_utf8_lossy(name));
        }
        assert_eq!(call(&effect, AUDIO_MASTER_CAN_DO, 0, 0, 0.0), 0);

        assert!(!ctx.halted() && !ctx.take_restart());
        assert_eq!(call(&effect, AUDIO_MASTER_IO_CHANGED, 0, 0, 0.0), 1);
        assert!(ctx.halted(), "the unit makes no further call with the old layout");
        assert!(ctx.take_restart() && !ctx.take_restart());
        assert!(ctx.halted(), "taking the restart does not resume the unit");
        ctx.clear_halted();
        assert!(!ctx.halted());

        assert_eq!(call(&effect, AUDIO_MASTER_UPDATE_DISPLAY, 0, 0, 0.0), 1);
        assert!(ctx.take_relist() && !ctx.take_relist());
        assert_eq!(call(&effect, AUDIO_MASTER_NEED_IDLE, 0, 0, 0.0), 1);
        assert!(ctx.take_need_idle() && !ctx.take_need_idle());

        assert_eq!(ctx.take_resize(), None);
        assert_eq!(call(&effect, AUDIO_MASTER_SIZE_WINDOW, 640, 480, 0.0), 0);
        assert_eq!(call(&effect, AUDIO_MASTER_SIZE_WINDOW, 800, 600, 0.0), 0);
        assert_eq!(call(&effect, AUDIO_MASTER_SIZE_WINDOW, 0, 600, 0.0), 0);
        assert_eq!(call(&effect, AUDIO_MASTER_SIZE_WINDOW, 800, -1, 0.0), 0);
        assert_eq!(ctx.take_resize(), Some((800, 600)), "the latest usable size, width and height together");
        assert_eq!(ctx.take_resize(), None);

        assert!(ctx.set_rate(0.0, 256).is_err() && ctx.set_rate(f64::NAN, 256).is_err() && ctx.set_rate(48_000.0, 0).is_err());
        assert_eq!(call(&effect, AUDIO_MASTER_GET_SAMPLE_RATE, 0, 0, 0.0), 44_100, "a refused rate changes nothing");
        ctx.set_rate(96_000.0, 64).unwrap();
        assert_eq!(call(&effect, AUDIO_MASTER_GET_SAMPLE_RATE, 0, 0, 0.0), 96_000);
        assert_eq!(call(&effect, AUDIO_MASTER_GET_BLOCK_SIZE, 0, 0, 0.0), 64);
        assert!(HostContext::new(-1.0, 64).is_err() && HostContext::new(48_000.0, -64).is_err());

        // SAFETY: the opening thread, opcodes that write a string.
        unsafe {
            assert_eq!(effect.string(EFF_GET_EFFECT_NAME, 0).as_deref(), Some("Fixture"));
            assert_eq!(effect.string(EFF_GET_VENDOR_STRING, 0), None, "a plugin that writes nothing");
            effect.close();
        }
        fixture::take_seen();
        fixture::take_calls();
    }

    #[test]
    fn a_module_without_a_vst2_entry_is_refused_and_a_missing_one_is_an_error() {
        // A system DLL exports neither entry name.
        let system = std::env::var("SystemRoot").unwrap();
        let version = Path::new(&system).join("System32").join("version.dll");
        match Vst2Module::load(&version) {
            Err(e) => assert!(e.contains("no VST2 entry"), "{e}"),
            Ok(_) => panic!("version.dll is not a VST2 plugin"),
        }
        assert!(Vst2Module::load(Path::new(r"C:\no-such-folder\missing.dll")).is_err());
    }
}
