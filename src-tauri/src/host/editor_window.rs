//! P10.0 plugin editor: a host-owned top-level Win32 window that an embedded (non-floating) plugin
//! GUI parents into. Format-agnostic — shared by both the CLAP (`clap.rs`) and VST3 (`clap::vst3_host`)
//! editor paths.
//!
//! The host window is OUR OWN top-level (owned by the main window for z-order), NOT reparented into
//! the WebView2 surface — so no airspace z-fight.

use std::sync::atomic::{AtomicBool, Ordering::Acquire, Ordering::Release};
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetClientRect, GetWindowLongPtrW, GetWindowRect, LoadCursorW, MsgWaitForMultipleObjectsEx,
    PeekMessageW, RegisterClassExW, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos,
    ShowWindow, TranslateMessage, CW_USEDEFAULT, GWLP_USERDATA, HWND_TOP, IDC_ARROW, MSG,
    MWMO_INPUTAVAILABLE, PM_REMOVE, QS_ALLINPUT, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, SW_HIDE, SW_SHOW, WINDOW_EX_STYLE, WM_CLOSE, WNDCLASSEXW, WS_CAPTION,
    WS_OVERLAPPED, WS_SYSMENU,
};

const HOST_WINDOW_CLASS: PCWSTR = w!("BleepLoopPluginHostWindow");
static HOST_CLASS_ONCE: std::sync::Once = std::sync::Once::new();

/// Register the editor-host window class once per process.
fn register_host_window_class() {
    HOST_CLASS_ONCE.call_once(|| {
        // SAFETY: one-shot class registration; the wndproc is a valid 'static fn and the class
        // name is a 'static wide literal. Failure is logged (CreateWindowExW then fails cleanly).
        unsafe {
            let hinst = GetModuleHandleW(None)
                .map(|m| HINSTANCE(m.0))
                .unwrap_or_default();
            let cursor = LoadCursorW(None, IDC_ARROW).unwrap_or_default();
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(host_wndproc),
                hInstance: hinst,
                hCursor: cursor,
                lpszClassName: HOST_WINDOW_CLASS,
                ..Default::default()
            };
            if RegisterClassExW(&wc) == 0 {
                log::warn!("[plugin_host] RegisterClassExW for the editor host window failed");
            }
        }
    });
}

/// Editor-host window proc. Only `WM_CLOSE` is special: the user clicked the close box → flag it
/// (the owner loop tears the editor down in order) and hide the window for instant feedback, but
/// do NOT destroy here (the plugin must `gui.destroy` its embedded child first).
unsafe extern "system" fn host_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_CLOSE {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const AtomicBool;
        if !ptr.is_null() {
            // SAFETY: the pointer is the `HostWindow`'s boxed flag, alive until the window is
            // destroyed (HostWindow::drop clears USERDATA before DestroyWindow).
            unsafe { (*ptr).store(true, Release) };
        }
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// A host-owned top-level editor window (RAII). The boxed `closed` flag has a stable address the
/// wndproc reads via `GWLP_USERDATA`; it is freed only after the window is destroyed.
pub(super) struct HostWindow {
    pub(super) hwnd: HWND,
    closed: Box<AtomicBool>,
}
impl HostWindow {
    pub(super) fn close_requested(&self) -> bool {
        self.closed.load(Acquire)
    }
}
impl Drop for HostWindow {
    fn drop(&mut self) {
        // SAFETY: `hwnd` was created on this (owner) thread and is dropped once. Clear USERDATA
        // first so an in-flight message can't read the about-to-free flag, then destroy.
        unsafe {
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Create the host-owned editor window sized so its CLIENT area is `width`×`height` (the plugin's
/// reported size).
///
/// `owner` (the main window, if known) is DELIBERATELY NOT passed as the Win32 owner anymore. This
/// window is created + pumped on the per-slot OWNER thread, but the main app window lives on the UI
/// (WebView2) thread — making it a cross-thread OWNED window. Hiding/destroying a cross-thread-owned
/// top-level window forces Windows to re-home activation to the owner via SYNCHRONOUS cross-thread
/// `SendMessage` (WM_ACTIVATE/WM_NCACTIVATE); combined with the owner loop stopping its message pump
/// the instant the editor closes, a JUCE plugin's teardown traffic deadlocked the owner↔UI pair and
/// froze the renderer (the close→reopen→disarm hang). Creating it OWNER-LESS removes that edge; the
/// only cost is the editor no longer pins above the app (it can fall behind). `drain_after_editor_
/// teardown` is the paired belt-and-suspenders. The param is kept for API stability / future pinning
/// via a non-owning SetWindowPos.
pub(super) fn create_host_window(
    width: u32,
    height: u32,
    owner: Option<HWND>,
) -> Result<HostWindow, String> {
    let _ = owner; // intentionally not the Win32 owner — see the doc note above
    register_host_window_class();
    let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU; // titlebar + close box, fixed size
    let ex = WINDOW_EX_STYLE(0);
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: width.max(1) as i32,
        bottom: height.max(1) as i32,
    };
    let closed = Box::new(AtomicBool::new(false));
    let flag_ptr = closed.as_ref() as *const AtomicBool;
    // SAFETY: FFI window creation on the owner thread; all pointers/handles are valid for the call.
    unsafe {
        let _ = AdjustWindowRectEx(&mut rect, style, false, ex);
        let w = (rect.right - rect.left).max(1);
        let h = (rect.bottom - rect.top).max(1);
        let hinst = GetModuleHandleW(None)
            .map(|m| HINSTANCE(m.0))
            .map_err(|e| format!("GetModuleHandleW: {e}"))?;
        let hwnd = CreateWindowExW(
            ex,
            HOST_WINDOW_CLASS,
            w!("BleepLoop Plugin"),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            w,
            h,
            None, // NO Win32 owner — a cross-thread owned window deadlocks owner↔UI on close (see doc)
            None,
            Some(hinst),
            None,
        )
        .map_err(|e| format!("CreateWindowExW: {e}"))?;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, flag_ptr as isize);
        let win = HostWindow { hwnd, closed };
        // `AdjustWindowRectEx` sizes the frame at the SYSTEM DPI; on a monitor scaled differently
        // (per-monitor-v2 process, mixed-DPI desktop) the caption is thicker than it computed and
        // the client area comes out short. Measure the real frame and correct it while the window
        // is still hidden, so the plugin's reported size is what it gets.
        let _ = set_client_size(hwnd, width, height);
        Ok(win)
    }
}

/// The window's current client-area size in pixels (0×0 if the handle is dead).
pub(super) fn client_size(hwnd: HWND) -> (u32, u32) {
    let mut client = RECT::default();
    // SAFETY: a plain query on a handle we own; a dead handle fails and leaves the zero rect.
    if unsafe { GetClientRect(hwnd, &mut client) }.is_err() {
        return (0, 0);
    }
    (
        (client.right - client.left).max(0) as u32,
        (client.bottom - client.top).max(0) as u32,
    )
}

/// Resize the host window so its CLIENT area is `width`×`height`, keeping its position and z-order.
/// The non-client frame is MEASURED (window rect minus client rect), not recomputed from styles, so
/// the result is correct at the window's actual DPI. This is the one place a plugin-initiated editor
/// resize lands for both formats: VST3 `IPlugFrame::resizeView` and the hosted-CLAP `request_resize`
/// call it, then the caller tells the plugin the size it GOT — which Windows may clamp below the
/// request (a captioned top-level window cannot outgrow the virtual screen: the default
/// `WM_GETMINMAXINFO` track size applies to `SetWindowPos` too). Returns the achieved client size,
/// or `None` when the Win32 calls failed (dead handle) and the window is as it was.
pub(super) fn set_client_size(hwnd: HWND, width: u32, height: u32) -> Option<(u32, u32)> {
    let (w, h) = (width.max(1) as i32, height.max(1) as i32);
    let mut win_rect = RECT::default();
    let mut client = RECT::default();
    // SAFETY: queries + one move-less SetWindowPos on a handle we own. Any thread may call
    // SetWindowPos; Win32 marshals it to the window's thread. Failures leave the window as it was
    // and report None.
    unsafe {
        if GetWindowRect(hwnd, &mut win_rect).is_err() || GetClientRect(hwnd, &mut client).is_err() {
            return None;
        }
        let frame_w = (win_rect.right - win_rect.left) - (client.right - client.left);
        let frame_h = (win_rect.bottom - win_rect.top) - (client.bottom - client.top);
        if SetWindowPos(
            hwnd,
            None,
            0,
            0,
            w + frame_w,
            h + frame_h,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        )
        .is_err()
        {
            return None;
        }
    }
    Some(client_size(hwnd))
}

/// Show a freshly-created host editor window and raise it to the FRONT once, at open.
///
/// The window is deliberately OWNER-LESS (`create_host_window` passes no Win32 owner — a
/// cross-thread owned window re-introduces the close→reopen→disarm deadlock; see its doc), which
/// also means it does NOT auto-pin above the app and would otherwise open BEHIND the main window.
/// Raise it to the top of the non-topmost z-order band and request foreground EXACTLY ONCE here.
/// This is a one-shot raise, NOT a persistent relationship: no Win32 owner and no `WS_EX_TOPMOST`,
/// so no owner/activation edge is created (the hang fix is preserved) and the editor can still
/// fall behind the app later when you click back into it — the intended, non-pinned behaviour.
/// The app process owns the foreground at open (the user just clicked the in-app editor control),
/// so the cross-thread `SetForegroundWindow` is permitted.
pub(super) fn show_host_window_front(hwnd: HWND) {
    // SAFETY: showing/raising our own freshly-created top-level window on the owner thread.
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        // z-order to the front of the non-topmost band without forcing activation, then request
        // foreground. HWND_TOP (not HWND_TOPMOST) = no permanent pin.
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
        let _ = SetForegroundWindow(hwnd);
    }
}

/// Drain + dispatch all pending Win32 messages for this thread. Must run regularly while a hosted
/// editor is open, or the embedded plugin UI freezes.
pub(super) fn pump_thread_messages() {
    let mut msg = MSG::default();
    // SAFETY: standard message pump; `msg` is a valid local for each call.
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Block up to `ms` for new thread input (returns immediately if input is already queued), so the
/// hosted-editor loop stays responsive without busy-spinning.
pub(super) fn wait_for_input(ms: u32) {
    // SAFETY: 0 wait handles; wake on any input or the timeout. Nothing is retained.
    unsafe {
        MsgWaitForMultipleObjectsEx(None, ms, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
    }
}

/// How long `drain_after_editor_teardown` keeps pumping.
const TEARDOWN_DRAIN: Duration = Duration::from_millis(100);

/// Keep pumping owner-thread Win32 messages for a short BOUNDED window right after an editor window
/// is torn down, so its `WM_DESTROY`/`WM_NCDESTROY` and any messages the plugin POSTED during its
/// own teardown are dispatched HERE (the owner thread) — instead of being left unanswered after the
/// owner loop falls back into its blocking `recv_timeout` branch (it only pumps while an editor is
/// open). Leaving them unserviced is what wedged the close→reopen→disarm path into an app freeze:
/// a JUCE plugin (e.g. Neural DSP) issues synchronous teardown traffic that, unpumped, never
/// completes. Bounded by a deadline (`TEARDOWN_DRAIN`), not a count of waits: a timed wait rounds
/// up to the timer tick Windows grants the process, which the app cannot count on being 1 ms
/// (measured 15.4 ms on the dev PC, where fifty 2 ms waits took ~770 ms). Each wait is for the time
/// left and returns early on new input, so the window is ~100 ms at any tick, overrun by at most
/// one tick, and a misbehaving plugin can't spin the owner forever. Pairs with the editor window
/// no longer being cross-thread OWNED (create_host_window passes no Win32 owner), which removes
/// the activation SendMessage edge.
pub(super) fn drain_after_editor_teardown() {
    let deadline = Instant::now() + TEARDOWN_DRAIN;
    loop {
        pump_thread_messages();
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        wait_for_input(left.as_micros().div_ceil(1000) as u32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

    /// The plugin asks for a client area and must get exactly that, at creation and on a later
    /// resize, whatever the non-client frame measures at this DPI.
    #[test]
    fn host_window_client_area_is_exact_at_creation_and_after_resize() {
        let win = create_host_window(640, 480, None).expect("create host window");
        assert_eq!(client_size(win.hwnd), (640, 480), "client area at creation");
        assert_eq!(set_client_size(win.hwnd, 800, 600), Some((800, 600)), "resize");
        assert_eq!(client_size(win.hwnd), (800, 600), "client area after resize");
        assert_eq!(set_client_size(win.hwnd, 300, 200), Some((300, 200)), "shrink");
        assert_eq!(client_size(win.hwnd), (300, 200), "client area after shrink");
        assert!(!win.close_requested());
    }

    /// The drain pumps for its whole window, dispatching what the plugin posts late in it, and the
    /// window does not stretch with the timer tick Windows grants the test process.
    #[test]
    fn teardown_drain_lasts_its_window_and_dispatches_late_posts() {
        let win = create_host_window(200, 100, None).expect("create host window");
        let hwnd = win.hwnd.0 as isize;
        let poster = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            // SAFETY: the window belongs to the test thread and outlives this join.
            unsafe { PostMessageW(Some(HWND(hwnd as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0)) }
                .expect("post WM_CLOSE");
        });
        let started = Instant::now();
        drain_after_editor_teardown();
        let took = started.elapsed();
        poster.join().unwrap();
        assert!(win.close_requested(), "a message posted 40 ms in was not dispatched");
        assert!(took >= TEARDOWN_DRAIN, "the drain ended after {took:?}, inside its window");
        assert!(took < TEARDOWN_DRAIN * 4, "the drain took {took:?}");
    }

    /// A dead handle fails cleanly instead of resizing something else or panicking.
    #[test]
    fn set_client_size_on_a_destroyed_window_is_false() {
        let win = create_host_window(200, 100, None).expect("create host window");
        let hwnd = win.hwnd;
        drop(win);
        assert_eq!(set_client_size(hwnd, 400, 300), None);
        assert_eq!(client_size(hwnd), (0, 0));
    }
}
