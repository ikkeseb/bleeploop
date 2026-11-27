//! The Linux stand-in for `editor_window.rs`: the same names, so the shared CLAP and VST3 owners
//! (`clap.rs`, `vst3.rs`, `engine_slot.rs`) compile unchanged. A plugin's own editor window is not
//! hosted on Linux yet: `create_host_window` refuses, so an editor open answers an error and the
//! drawer's parameters stay the way to play the plugin. An owner has no Win32 queue to pump; its
//! idle wait is a plain sleep.

use std::ffi::c_void;
use std::time::Duration;

/// A native window handle, in the shape the Windows `HWND` has (`.0` is the pointer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct HWND(pub *mut c_void);

/// A host-owned editor window. None is ever created on Linux.
pub(super) struct HostWindow {
    pub(super) hwnd: HWND,
}

impl HostWindow {
    pub(super) fn close_requested(&self) -> bool {
        false
    }
}

pub(super) fn create_host_window(_width: u32, _height: u32) -> Result<HostWindow, String> {
    Err("plugin editor windows are not hosted on Linux yet".to_string())
}

pub(super) fn client_size(_hwnd: HWND) -> (u32, u32) {
    (0, 0)
}

pub(super) fn set_client_size(_hwnd: HWND, _width: u32, _height: u32) -> Option<(u32, u32)> {
    None
}

pub(super) fn show_host_window_front(_hwnd: HWND) {}

/// Nothing to dispatch: no window of the owner's exists on Linux.
pub(super) fn pump_thread_messages() {}

/// The owner's idle wait between turns.
pub(super) fn wait_for_input(ms: u32) {
    std::thread::sleep(Duration::from_millis(u64::from(ms)));
}

pub(super) fn drain_after_editor_teardown() {}

/// The thread a window belongs to, as the Windows call answers it: never the caller's, so a
/// `request_resize` is queued for the owner's turn (where `set_client_size` refuses it).
#[allow(non_snake_case)]
pub(super) unsafe fn GetWindowThreadProcessId(_hwnd: HWND, _pid: Option<*mut u32>) -> u32 {
    0
}

#[allow(non_snake_case)]
pub(super) unsafe fn GetCurrentThreadId() -> u32 {
    1
}
