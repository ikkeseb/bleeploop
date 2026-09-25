//! A VST3 edit controller implemented in Rust through the real `IEditController` vtable
//! (`ComWrapper`), holding its component handler the way the SDK's `EditController` does (add-ref'd),
//! driven by the production parameter listing and editor open. Proves that a malformed parameter
//! count is an error rather than a process abort, and that an editor open never replaces the
//! load-time component handler. No DLL or plugin GUI required; no window is shown.
use super::*;
use std::sync::atomic::AtomicUsize;
use vst3::Steinberg::{char16, int16, IBStream, TBool};

pub(super) struct FixtureController {
    param_count: AtomicI32,
    /// The handler the controller currently holds, and how often the host set one.
    pub(super) handler: Mutex<Option<ComPtr<IComponentHandler>>>,
    handler_sets: AtomicUsize,
    /// Every `setParamNormalized` the host made (the engine-mode tests' controller mirror).
    pub(super) set_normalized: Mutex<Vec<(ParamID, ParamValue)>>,
}

impl FixtureController {
    pub(super) fn new(param_count: i32) -> Self {
        Self {
            param_count: AtomicI32::new(param_count),
            handler: Mutex::new(None),
            handler_sets: AtomicUsize::new(0),
            set_normalized: Mutex::new(Vec::new()),
        }
    }
    fn held_handler(&self) -> Option<usize> {
        self.handler.lock().unwrap().as_ref().map(|h| h.as_ptr() as usize)
    }
}

impl Class for FixtureController {
    type Interfaces = (IEditController,);
}

impl IPluginBaseTrait for FixtureController {
    unsafe fn initialize(&self, _context: *mut FUnknown) -> tresult {
        kResultOk
    }
    unsafe fn terminate(&self) -> tresult {
        kResultOk
    }
}

impl IEditControllerTrait for FixtureController {
    unsafe fn setComponentState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }
    unsafe fn setState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }
    unsafe fn getState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }
    unsafe fn getParameterCount(&self) -> int32 {
        self.param_count.load(Relaxed)
    }
    /// Answers every index below a small bound, so a malformed count stays cheap to walk; the
    /// listing must refuse the count before it sizes anything from it.
    unsafe fn getParameterInfo(&self, index: int32, info: *mut ParameterInfo) -> tresult {
        if !(0..16).contains(&index) {
            return kResultFalse;
        }
        // SAFETY: the host passes a valid ParameterInfo.
        let info = &mut *info;
        info.id = 1000 + index as u32;
        info.defaultNormalizedValue = 0.5;
        info.flags = 0;
        kResultOk
    }
    unsafe fn getParamStringByValue(
        &self,
        _id: ParamID,
        _value: ParamValue,
        _string: *mut String128,
    ) -> tresult {
        kResultFalse
    }
    unsafe fn getParamValueByString(
        &self,
        _id: ParamID,
        _string: *mut TChar,
        _value: *mut ParamValue,
    ) -> tresult {
        kResultFalse
    }
    unsafe fn normalizedParamToPlain(&self, _id: ParamID, value: ParamValue) -> ParamValue {
        value
    }
    unsafe fn plainParamToNormalized(&self, _id: ParamID, value: ParamValue) -> ParamValue {
        value
    }
    unsafe fn getParamNormalized(&self, _id: ParamID) -> ParamValue {
        0.25
    }
    unsafe fn setParamNormalized(&self, id: ParamID, value: ParamValue) -> tresult {
        self.set_normalized.lock().unwrap().push((id, value));
        kResultOk
    }
    unsafe fn setComponentHandler(&self, handler: *mut IComponentHandler) -> tresult {
        self.handler_sets.fetch_add(1, Relaxed);
        // Like the SDK: release the old handler, add-ref the new one.
        *self.handler.lock().unwrap() = ComRef::from_raw(handler).map(|h| h.to_com_ptr());
        kResultOk
    }
    unsafe fn createView(&self, _name: FIDString) -> *mut IPlugView {
        ComWrapper::new(RefusingView)
            .to_com_ptr::<IPlugView>()
            .map_or(std::ptr::null_mut(), ComPtr::into_raw)
    }
}

/// A view that refuses `attached`, so an editor open walks every step up to the attach and then
/// fails before its host window is ever shown.
struct RefusingView;
impl Class for RefusingView {
    type Interfaces = (IPlugView,);
}
impl IPlugViewTrait for RefusingView {
    unsafe fn isPlatformTypeSupported(&self, _type: FIDString) -> tresult {
        kResultTrue
    }
    unsafe fn attached(&self, _parent: *mut c_void, _type: FIDString) -> tresult {
        kResultFalse
    }
    unsafe fn removed(&self) -> tresult {
        kResultOk
    }
    unsafe fn onWheel(&self, _distance: f32) -> tresult {
        kResultOk
    }
    unsafe fn onKeyDown(&self, _key: char16, _code: int16, _mods: int16) -> tresult {
        kResultOk
    }
    unsafe fn onKeyUp(&self, _key: char16, _code: int16, _mods: int16) -> tresult {
        kResultOk
    }
    unsafe fn getSize(&self, size: *mut ViewRect) -> tresult {
        *size = ViewRect {
            left: 0,
            top: 0,
            right: 320,
            bottom: 200,
        };
        kResultOk
    }
    unsafe fn onSize(&self, _new_size: *mut ViewRect) -> tresult {
        kResultOk
    }
    unsafe fn onFocus(&self, _state: TBool) -> tresult {
        kResultOk
    }
    unsafe fn setFrame(&self, _frame: *mut IPlugFrame) -> tresult {
        kResultOk
    }
    unsafe fn canResize(&self) -> tresult {
        kResultFalse
    }
    unsafe fn checkSizeConstraint(&self, _rect: *mut ViewRect) -> tresult {
        kResultOk
    }
}

fn controller(
    param_count: i32,
) -> (ComWrapper<FixtureController>, Option<ComPtr<IEditController>>) {
    let obj = ComWrapper::new(FixtureController::new(param_count));
    let ptr = obj.to_com_ptr::<IEditController>();
    (obj, ptr)
}

/// A controller that reports a count no real plugin has (or a negative one) gets an error, not
/// an allocation sized from it; a sane count still lists every parameter, live values included.
#[test]
fn a_malformed_parameter_count_is_an_error_not_an_abort() {
    for bogus in [i32::MAX, 1 << 20, -1] {
        let (_obj, ctl) = controller(bogus);
        let err = list_vst3_params(&ctl).expect_err("a malformed count must be refused");
        assert!(err.contains("parameter count"), "{bogus}: {err}");
    }
    let (_obj, ctl) = controller(3);
    let params = list_vst3_params(&ctl).expect("a sane count lists");
    assert_eq!(params.iter().map(|p| p.id).collect::<Vec<_>>(), vec![1000, 1001, 1002]);
    assert!(params.iter().all(|p| p.default_value == 0.5 && p.value == 0.25));
}

/// The owner sets ONE component handler at load. An editor open (here refused at the attach,
/// after createView, setFrame, getSize and the host window) must leave exactly that handler in
/// the controller: an editor-owned replacement would be dropped at close while the controller
/// still points at it.
#[test]
fn an_editor_open_leaves_the_load_time_component_handler_in_place() {
    let (obj, ctl) = controller(3);
    let load_handler = ComWrapper::new(LfComponentHandler {
        event_tx: Arc::new(Mutex::new(rtrb::RingBuffer::<PluginEvent>::new(4).0)),
        param_changed: Box::new(|_, _| {}),
        restart: Arc::new(RestartFlags::default()),
    });
    let hp = load_handler.to_com_ptr::<IComponentHandler>().expect("IComponentHandler");
    // SAFETY: both pointers come from live ComWrappers held for the whole test.
    unsafe {
        ctl.as_ref().unwrap().setComponentHandler(hp.as_ptr());
    }
    assert_eq!(obj.held_handler(), Some(hp.as_ptr() as usize));

    let err = vst3_editor_open(&ctl, 0, 0).err().expect("the fixture view refuses to attach");
    assert!(err.contains("attached"), "failed at the attach step, not before it: {err}");
    assert_eq!(obj.handler_sets.load(Relaxed), 1, "only the load set a component handler");
    assert_eq!(
        obj.held_handler(),
        Some(hp.as_ptr() as usize),
        "the controller still holds the load-time handler"
    );
}
