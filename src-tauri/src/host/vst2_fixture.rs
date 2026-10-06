//! A VST2 effect implemented in Rust behind the real `AEffect` layout and a real entry function, so
//! `open_effect` and the host callback are exercised through the binary interface with no DLL. The
//! entry builds whatever `Shape` the test set for its thread (a valid stereo effect by default, or
//! one with a field this host must refuse), calls the host back from inside the "constructor" the
//! ways real plugins do, and records what it was told and every dispatcher call it got.
use super::super::vst2_abi::*;
use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

/// What the next entry call on this thread builds.
#[derive(Clone, Copy)]
pub(crate) struct Shape {
    /// The entry returns no effect at all.
    pub(crate) null: bool,
    pub(crate) magic: i32,
    pub(crate) flags: i32,
    pub(crate) inputs: i32,
    pub(crate) outputs: i32,
    pub(crate) params: i32,
    pub(crate) programs: i32,
    pub(crate) delay: i32,
    pub(crate) unique_id: i32,
    /// Whether each function pointer is there.
    pub(crate) dispatcher: bool,
    pub(crate) process: bool,
    pub(crate) process_replacing: bool,
    pub(crate) set_parameter: bool,
    pub(crate) get_parameter: bool,
    /// What `effGetEffectName` and `effGetProductString` write (empty: nothing).
    pub(crate) effect_name: &'static str,
    pub(crate) product: &'static str,
    /// `effGetPlugCategory`'s answer.
    pub(crate) category: isize,
    /// Runs inside the entry, between its first callback and the rest.
    pub(crate) during_entry: Option<fn()>,
}

impl Default for Shape {
    fn default() -> Self {
        Shape {
            null: false,
            magic: EFFECT_MAGIC,
            flags: EFF_FLAGS_CAN_REPLACING,
            inputs: 2,
            outputs: 2,
            params: 3,
            programs: 1,
            delay: 0,
            unique_id: 0x4c66_4678,
            dispatcher: true,
            process: true,
            process_replacing: true,
            set_parameter: true,
            get_parameter: true,
            effect_name: "Fixture",
            product: "Fixture Product",
            category: 1,
            during_entry: None,
        }
    }
}

/// One instance: the `AEffect` first, so the effect's pointer is the instance's.
#[repr(C)]
struct Instance {
    effect: AEffect,
    shape: Shape,
}

thread_local! {
    static NEXT: Cell<Option<Shape>> = const { Cell::new(None) };
    /// What the host answered the entry's callbacks, in order.
    static SEEN: RefCell<Vec<isize>> = const { RefCell::new(Vec::new()) };
    /// Every dispatcher call on this thread: the opcode, and whether `resvd1` was set by then.
    static CALLS: RefCell<Vec<(i32, bool)>> = const { RefCell::new(Vec::new()) };
}

/// `setParameter` and `getParameter` calls on any fixture instance, from any thread.
static PARAMETER_CALLS: AtomicUsize = AtomicUsize::new(0);

/// Run `f` with `shape` as what the entry builds on this thread; the outer shape comes back after.
pub(crate) fn with_shape<R>(shape: Shape, f: impl FnOnce() -> R) -> R {
    let outer = NEXT.replace(Some(shape));
    let result = f();
    NEXT.set(outer);
    result
}

/// What the host answered the entries run on this thread since the last take: per entry, the sample
/// rate asked with no effect, (then `during_entry`), the rate asked with the still unbound effect,
/// and the block size asked both ways.
pub(crate) fn take_seen() -> Vec<isize> {
    SEEN.take()
}

/// The dispatcher calls this thread's instances got since the last take.
pub(crate) fn take_calls() -> Vec<(i32, bool)> {
    CALLS.take()
}

pub(crate) fn parameter_calls() -> usize {
    PARAMETER_CALLS.load(Relaxed)
}

/// The fixture's `VSTPluginMain`.
pub(crate) unsafe extern "C" fn entry(host: HostCallback) -> *mut AEffect {
    let shape = NEXT.get().unwrap_or_default();
    let ask = |effect: *mut AEffect, opcode: i32| {
        // SAFETY: the host's callback, with an opcode that reads no pointer.
        let answer = unsafe { host(effect, opcode, 0, 0, std::ptr::null_mut(), 0.0) };
        SEEN.with_borrow_mut(|seen| seen.push(answer));
    };
    // A constructor asking before it has an effect to name.
    ask(std::ptr::null_mut(), AUDIO_MASTER_GET_SAMPLE_RATE);
    if let Some(during) = shape.during_entry {
        during();
    }
    let instance = Box::into_raw(Box::new(Instance {
        effect: AEffect {
            magic: shape.magic,
            dispatcher: shape.dispatcher.then_some(dispatcher as DispatcherFn),
            process: shape.process.then_some(process as ProcessFn),
            set_parameter: shape.set_parameter.then_some(set_parameter as SetParameterFn),
            get_parameter: shape.get_parameter.then_some(get_parameter as GetParameterFn),
            num_programs: shape.programs,
            num_params: shape.params,
            num_inputs: shape.inputs,
            num_outputs: shape.outputs,
            flags: shape.flags,
            resvd1: 0,
            resvd2: 0,
            initial_delay: shape.delay,
            real_qualities: 0,
            off_qualities: 0,
            io_ratio: 1.0,
            object: std::ptr::null_mut(),
            user: std::ptr::null_mut(),
            unique_id: shape.unique_id,
            version: 1,
            process_replacing: shape.process_replacing.then_some(process as ProcessFn),
            process_double_replacing: None,
            future: [0; 56],
        },
        shape,
    }));
    let effect = instance.cast::<AEffect>();
    // SAFETY: the instance just boxed; `object` is the plugin's own pointer to itself.
    unsafe { (*effect).object = instance.cast() };
    // A constructor asking with its effect, which the host has not bound to a context yet.
    ask(effect, AUDIO_MASTER_GET_SAMPLE_RATE);
    ask(std::ptr::null_mut(), AUDIO_MASTER_GET_BLOCK_SIZE);
    ask(effect, AUDIO_MASTER_GET_BLOCK_SIZE);
    if shape.null {
        // SAFETY: the instance boxed above, handed to nobody.
        drop(unsafe { Box::from_raw(instance) });
        return std::ptr::null_mut();
    }
    effect
}

unsafe extern "C" fn dispatcher(effect: *mut AEffect, opcode: i32, _index: i32, _value: isize, ptr: *mut c_void, _opt: f32) -> isize {
    // SAFETY: the host passes the effect this fixture made, whose pointer is its instance's.
    let (bound, shape) = unsafe { ((*effect).resvd1 != 0, (*effect.cast::<Instance>()).shape) };
    CALLS.with_borrow_mut(|calls| calls.push((opcode, bound)));
    let write = |text: &str| {
        if !text.is_empty() {
            // SAFETY: the host's string buffer, far larger than any fixture name plus its NUL.
            unsafe {
                std::ptr::copy_nonoverlapping(text.as_ptr(), ptr.cast::<u8>(), text.len());
                *ptr.cast::<u8>().add(text.len()) = 0;
            }
        }
        isize::from(!text.is_empty())
    };
    match opcode {
        EFF_CLOSE => {
            // SAFETY: `effClose` frees the instance; the host never calls it again.
            drop(unsafe { Box::from_raw(effect.cast::<Instance>()) });
            0
        }
        EFF_GET_EFFECT_NAME => write(shape.effect_name),
        EFF_GET_PRODUCT_STRING => write(shape.product),
        EFF_GET_PLUG_CATEGORY => shape.category,
        // `effOpen` answers 0, as real plugins do: it is not a failure.
        _ => 0,
    }
}

unsafe extern "C" fn process(_effect: *mut AEffect, _inputs: *mut *mut f32, _outputs: *mut *mut f32, _frames: i32) {}

unsafe extern "C" fn set_parameter(_effect: *mut AEffect, _index: i32, _value: f32) {
    PARAMETER_CALLS.fetch_add(1, Relaxed);
}

unsafe extern "C" fn get_parameter(_effect: *mut AEffect, _index: i32) -> f32 {
    PARAMETER_CALLS.fetch_add(1, Relaxed);
    0.0
}
