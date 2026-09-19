//! A VST3 `IPlugView` implemented in Rust through the real vtable (`ComWrapper`), recording every
//! `onSize` it receives, driven against the production `LfPlugFrame` over its COM pointer and a
//! real host window. Proves the plugin-initiated resize contract: `resizeView` resizes the host
//! window's CLIENT area to the requested size and then calls `onSize` with exactly that size — and
//! is refused (no `onSize`) before the window exists. No DLL or plugin GUI required.
use super::super::super::editor_window::{client_size, create_host_window};
use super::*;
use vst3::Steinberg::{char16, int16, TBool};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
};

struct FixtureView {
    on_size: Mutex<Vec<(i32, i32, i32, i32)>>,
}
impl Class for FixtureView {
    type Interfaces = (IPlugView,);
}
impl IPlugViewTrait for FixtureView {
    unsafe fn isPlatformTypeSupported(&self, _type: FIDString) -> tresult {
        kResultOk
    }
    unsafe fn attached(&self, _parent: *mut c_void, _type: FIDString) -> tresult {
        kResultOk
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
            right: 400,
            bottom: 300,
        };
        kResultOk
    }
    unsafe fn onSize(&self, new_size: *mut ViewRect) -> tresult {
        let r = *new_size;
        self.on_size
            .lock()
            .unwrap()
            .push((r.left, r.top, r.right, r.bottom));
        kResultOk
    }
    unsafe fn onFocus(&self, _state: TBool) -> tresult {
        kResultOk
    }
    unsafe fn setFrame(&self, _frame: *mut IPlugFrame) -> tresult {
        kResultOk
    }
    unsafe fn canResize(&self) -> tresult {
        kResultTrue
    }
    unsafe fn checkSizeConstraint(&self, _rect: *mut ViewRect) -> tresult {
        kResultOk
    }
}

fn fixture() -> (ComWrapper<FixtureView>, ComWrapper<LfPlugFrame>) {
    (
        ComWrapper::new(FixtureView {
            on_size: Mutex::new(Vec::new()),
        }),
        ComWrapper::new(LfPlugFrame::new()),
    )
}

fn rect(w: i32, h: i32) -> ViewRect {
    ViewRect {
        left: 0,
        top: 0,
        right: w,
        bottom: h,
    }
}

/// `setFrame` happens before the host window exists (editor_open steps 4 vs 6): a resize in that
/// gap is refused and the view is NOT told a size it did not get.
#[test]
fn resize_before_the_window_exists_is_refused_without_on_size() {
    let (view, frame) = fixture();
    let frame_ptr = frame.to_com_ptr::<IPlugFrame>().expect("IPlugFrame");
    let view_ptr = view.to_com_ptr::<IPlugView>().expect("IPlugView");
    let mut wanted = rect(640, 480);
    // SAFETY: both pointers come from live ComWrappers held for the whole call.
    let res = unsafe { frame_ptr.resizeView(view_ptr.as_ptr(), &mut wanted) };
    assert_eq!(res, kResultFalse);
    assert!(view.on_size.lock().unwrap().is_empty(), "no onSize without a window");
}

/// With a host window: the client area takes the requested size and `onSize` reports exactly it,
/// once per request — grow and shrink. Sizes fit any desktop a CI runner offers (≥ 1024×768).
#[test]
fn resize_view_resizes_the_client_area_then_reports_it_with_on_size() {
    let (view, frame) = fixture();
    let win = create_host_window(400, 300, None).expect("host window");
    frame.hwnd.store(win.hwnd.0 as isize, Release);
    let frame_ptr = frame.to_com_ptr::<IPlugFrame>().expect("IPlugFrame");
    let view_ptr = view.to_com_ptr::<IPlugView>().expect("IPlugView");

    for (w, h) in [(640, 480), (800, 560), (320, 240)] {
        let mut wanted = rect(w, h);
        // SAFETY: as above; the window outlives the call.
        let res = unsafe { frame_ptr.resizeView(view_ptr.as_ptr(), &mut wanted) };
        assert_eq!(res, kResultOk, "resizeView({w}x{h})");
        assert_eq!(client_size(win.hwnd), (w as u32, h as u32), "client area after {w}x{h}");
    }
    assert_eq!(
        *view.on_size.lock().unwrap(),
        vec![(0, 0, 640, 480), (0, 0, 800, 560), (0, 0, 320, 240)],
        "one onSize per granted resize, carrying the granted size"
    );
}

/// A request larger than the virtual screen: Windows clamps the window, the call still succeeds,
/// and `onSize` carries the size the view REALLY has (never the unreachable request).
#[test]
fn oversize_request_is_clamped_and_the_view_is_told_the_real_size() {
    let (view, frame) = fixture();
    let win = create_host_window(400, 300, None).expect("host window");
    frame.hwnd.store(win.hwnd.0 as isize, Release);
    let frame_ptr = frame.to_com_ptr::<IPlugFrame>().expect("IPlugFrame");
    let view_ptr = view.to_com_ptr::<IPlugView>().expect("IPlugView");
    // SAFETY: plain metrics queries.
    let (sw, sh) = unsafe {
        (
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    let (w, h) = (sw + 400, sh + 400);
    let mut wanted = rect(w, h);
    // SAFETY: as above.
    let res = unsafe { frame_ptr.resizeView(view_ptr.as_ptr(), &mut wanted) };
    assert_eq!(res, kResultOk, "a clamped resize still succeeds");
    let (got_w, got_h) = client_size(win.hwnd);
    assert!(got_w >= 400 && got_h >= 300, "window grew from 400x300, got {got_w}x{got_h}");
    assert!(got_w as i32 <= w && got_h as i32 <= h, "never beyond the request");
    assert_eq!(
        *view.on_size.lock().unwrap(),
        vec![(0, 0, got_w as i32, got_h as i32)],
        "onSize carries the achieved size"
    );
}

/// Null pointers from a misbehaving plugin are refused, never dereferenced.
#[test]
fn null_view_or_rect_is_refused() {
    let (view, frame) = fixture();
    let win = create_host_window(400, 300, None).expect("host window");
    frame.hwnd.store(win.hwnd.0 as isize, Release);
    let frame_ptr = frame.to_com_ptr::<IPlugFrame>().expect("IPlugFrame");
    let view_ptr = view.to_com_ptr::<IPlugView>().expect("IPlugView");
    let mut wanted = rect(640, 480);
    // SAFETY: the production method must handle nulls; nothing else is dereferenced.
    unsafe {
        assert_eq!(frame_ptr.resizeView(std::ptr::null_mut(), &mut wanted), kResultFalse);
        assert_eq!(frame_ptr.resizeView(view_ptr.as_ptr(), std::ptr::null_mut()), kResultFalse);
    }
    assert_eq!(client_size(win.hwnd), (400, 300), "window untouched");
    assert!(view.on_size.lock().unwrap().is_empty());
}
