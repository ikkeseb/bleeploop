//! OWNS: port liveness (`docs/plans/native-midi.md` decision 10): the Windows notifications that a MIDI
//! interface arrived or went away, and what the port thread does about them. midir has no liveness
//! signal on WinMM, so without these a port unplugged and replugged inside one poll keeps its dead
//! connection.
//!
//! - **Registered before the first enumeration** (`CM_Register_Notification`, no window pump), on both
//!   classes a WinMM port may arrive on under `wdmaud2`: [`DEVINTERFACE_MIDI_INPUT`] and the MIDI 2.0
//!   endpoint class [`MIDI_ENDPOINT_CLASS`]. Unregistered when the port thread stops.
//! - **The callback only records and wakes.** It runs on a system thread-pool thread: it copies the
//!   interface path, sends one [`Wake`] down the port thread's channel and returns. It never calls
//!   WinMM, never takes [`super::Core`]'s lock, never unregisters (that would deadlock).
//! - **On wake** ([`Watch`]): a removal whose path is an open connection's invalidates that connection
//!   (its [`Generation`] bumps, so its late callbacks are ignored; `Core::port_gone` releases its notes
//!   and HOLD) and keeps the port closed until WinMM stops listing it or an arrival names it again; an
//!   arrival re-enumerates on a short backoff schedule ([`retry_wait`]), each pass opening what the
//!   enumeration confirms and retrying a failed open. Duplicate notifications are harmless. The 1 s
//!   poll (`ports::POLL`) stays as the backstop.
//!
//! Unknown, and untested on hardware (this machine has no MIDI input port, step 0): which class a
//! controller's arrival comes on, and whether the notification's path equals midir's id (the WinMM
//! device-interface path; compared ignoring ASCII case). **Next check, with a real controller** in the
//! running app: plug, unplug and replug it, and read the `[midi]` lines. `interface arrived/removed:
//! <path>` beside `opened input <name> as <id>` says which class it came on (the `{guid}` in the path)
//! and whether the paths match. A removal that names the open port's path and is followed by
//! `input disconnected` means liveness works. No notice at all means the port's class is neither of
//! the two (register the class its id ends in, or `CM_NOTIFY_FILTER_FLAG_ALL_INTERFACE_CLASSES`). A
//! removal with another path than the open port's means the paths differ: matching needs the device
//! instance (`CM_Get_Device_Interface_Property`, `DEVPKEY_Device_InstanceId`) on both sides. Also log
//! the id across an app restart and a replug into another USB socket: whether it is stable decides
//! how far a binding's stored id can be trusted (decision 8).

use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use windows::core::GUID;
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Register_Notification, CM_Unregister_Notification, CM_NOTIFY_ACTION, CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL,
    CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL, CM_NOTIFY_EVENT_DATA, CM_NOTIFY_FILTER, CM_NOTIFY_FILTER_0_0, CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE,
    CR_SUCCESS, HCMNOTIFICATION,
};
use windows::Win32::Media::Audio::DEVINTERFACE_MIDI_INPUT;

use super::ports::POLL;

/// The Windows MIDI Services endpoint interface class: the service's own endpoints came up on it with no
/// WinMM port (step 0, observed 2026-10-09). Not in the windows crate.
pub(crate) const MIDI_ENDPOINT_CLASS: GUID = GUID::from_u128(0xe7cce071_3c03_423f_88d3_f1045d02552b);

/// What wakes the port thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Wake {
    /// The host stops: close every port and return.
    Stop,
    /// A MIDI interface arrived at this path.
    Arrived(String),
    /// A MIDI interface at this path went away.
    Removed(String),
}

/// Interface paths are case-insensitive, and the notification and WinMM may spell one differently.
pub(crate) fn same_path(a: &str, b: &str) -> bool {
    !a.is_empty() && a.eq_ignore_ascii_case(b)
}

/// The arrival schedule: the waits before each re-enumeration after an arrival (an interface often
/// arrives before WinMM lists its port, and an open can fail while another program holds the port).
const RETRY_MS: [u64; 5] = [50, 100, 200, 400, 800];

/// The wait before re-enumeration `attempt` (from 0) after an arrival; `None` once the schedule is
/// spent (the poll takes over).
pub(crate) fn retry_wait(attempt: u32) -> Option<Duration> {
    RETRY_MS.get(attempt as usize).map(|&ms| Duration::from_millis(ms))
}

/// What the notifications left for the port thread: the paths a removal closed, and the arrival
/// schedule's progress. Pure; the port thread applies it.
#[derive(Debug, Default)]
pub(crate) struct Watch {
    /// Paths a removal closed. WinMM may still list them for a while: not reopened until an
    /// enumeration no longer lists them or an arrival names them again.
    removed: Vec<String>,
    /// The next step of the arrival schedule, while one runs.
    retry: Option<u32>,
}

impl Watch {
    /// A notification. Returns the indices into `open` (the open connections' paths) it invalidates.
    pub(crate) fn notice(&mut self, wake: &Wake, open: &[&str]) -> Vec<usize> {
        match wake {
            Wake::Removed(path) => {
                let hit: Vec<usize> = (0..open.len()).filter(|&i| same_path(open[i], path)).collect();
                if !hit.is_empty() && !self.removed.iter().any(|r| same_path(r, path)) {
                    self.removed.push(path.clone());
                }
                hit
            }
            Wake::Arrived(path) => {
                self.removed.retain(|r| !same_path(r, path));
                self.retry = Some(0);
                Vec::new()
            }
            Wake::Stop => Vec::new(),
        }
    }

    /// An enumeration listed these paths: a removed path it no longer lists may open again.
    pub(crate) fn listed(&mut self, present: &[&str]) {
        self.removed.retain(|r| present.iter().any(|p| same_path(p, r)));
    }

    /// Whether the port at `path` may open (no removal closed it while it is still listed).
    pub(crate) fn may_open(&self, path: &str) -> bool {
        !self.removed.iter().any(|r| same_path(r, path))
    }

    /// The wait before the next enumeration: the arrival schedule while it runs, then [`POLL`].
    pub(crate) fn next_wait(&mut self) -> Duration {
        match self.retry.and_then(|attempt| retry_wait(attempt).map(|wait| (attempt, wait))) {
            Some((attempt, wait)) => {
                self.retry = Some(attempt + 1);
                wait
            }
            None => {
                self.retry = None;
                POLL
            }
        }
    }
}

/// One connection's generation. Its midir callback captures the generation it was opened in and drops
/// every message once that is no longer current, so a connection the port thread has invalidated runs
/// nothing more, whatever WinMM still delivers before the close completes.
#[derive(Clone, Debug, Default)]
pub(crate) struct Generation(Arc<AtomicU32>);

impl Generation {
    pub(crate) fn current(&self) -> u32 {
        self.0.load(Ordering::Acquire)
    }

    /// Whether a callback opened in generation `ticket` may still deliver.
    pub(crate) fn admits(&self, ticket: u32) -> bool {
        self.current() == ticket
    }

    pub(crate) fn invalidate(&self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}

/// The live notification registrations. Dropping it unregisters them (Windows waits for a callback
/// in flight to return) and only then frees the channel the callbacks send on.
pub(crate) struct Registration {
    handles: Vec<HCMNOTIFICATION>,
    context: *mut Sender<Wake>,
}

impl Registration {
    /// Register on both MIDI interface classes; `None` (logged) when neither registers, and the poll
    /// alone follows the ports.
    pub(crate) fn new(wake: Sender<Wake>) -> Option<Registration> {
        // The callbacks share the sender from several pool threads at once.
        fn sync<T: Sync>() {}
        sync::<Sender<Wake>>();
        let context = Box::into_raw(Box::new(wake));
        let mut handles = Vec::new();
        for class in [DEVINTERFACE_MIDI_INPUT, MIDI_ENDPOINT_CLASS] {
            let mut filter = CM_NOTIFY_FILTER {
                cbSize: std::mem::size_of::<CM_NOTIFY_FILTER>() as u32,
                FilterType: CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE,
                ..Default::default()
            };
            filter.u.DeviceInterface = CM_NOTIFY_FILTER_0_0 { ClassGuid: class };
            let mut handle = HCMNOTIFICATION::default();
            // SAFETY: `filter` and `handle` outlive the call; `context` stays valid until every handle is
            // unregistered (`Drop`), and `on_notify` only reads it.
            let r = unsafe { CM_Register_Notification(&filter, Some(context as *const c_void), Some(on_notify), &mut handle) };
            if r == CR_SUCCESS {
                handles.push(handle);
            } else {
                log::warn!("[midi] no interface notifications for {class:?}: CONFIGRET {}", r.0);
            }
        }
        if handles.is_empty() {
            // SAFETY: nothing registered, so no callback holds it.
            drop(unsafe { Box::from_raw(context) });
            return None;
        }
        Some(Registration { handles, context })
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        for handle in self.handles.drain(..) {
            // SAFETY: registered by `new`, unregistered once, never from a callback.
            unsafe { CM_Unregister_Notification(handle) };
        }
        // SAFETY: every registration is gone, so no callback can still read it.
        drop(unsafe { Box::from_raw(self.context) });
    }
}

/// The notification callback, on a system thread-pool thread: copy the path, wake the port thread.
unsafe extern "system" fn on_notify(
    _handle: HCMNOTIFICATION,
    context: *const c_void,
    action: CM_NOTIFY_ACTION,
    data: *const CM_NOTIFY_EVENT_DATA,
    size: u32,
) -> u32 {
    // An unwind out of an `extern "system"` callback would abort the process.
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let removed = action == CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL;
        if context.is_null() || data.is_null() || !(removed || action == CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL) {
            return;
        }
        // SAFETY: Windows hands `size` readable bytes at `data` for an interface event; the path is
        // NUL-terminated inside them, and the scan stops at their end regardless.
        let path = unsafe {
            let link = std::ptr::addr_of!((*data).u.DeviceInterface.SymbolicLink) as *const u16;
            let units = (size as usize).saturating_sub(link as usize - data as usize) / 2;
            let wide = std::slice::from_raw_parts(link, units);
            String::from_utf16_lossy(&wide[..wide.iter().position(|&c| c == 0).unwrap_or(units)])
        };
        // SAFETY: `context` is the `Sender` `Registration::new` boxed, alive while registered.
        let wake = unsafe { &*(context as *const Sender<Wake>) };
        let _ = wake.send(if removed { Wake::Removed(path) } else { Wake::Arrived(path) });
    }));
    0 // ERROR_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    const PEDAL: &str = r"\\?\USB#VID_1234&PID_0001#5&1#{504be32c-ccf6-4d2c-b73f-6f8b3747e22b}\Pedal";

    // Decision 10: a removal invalidates exactly the connections on its path (both ports of a
    // multi-port device), whatever case Windows spells it in; another path invalidates nothing.
    #[test]
    fn a_removal_invalidates_the_connections_on_its_path_only() {
        let mut w = Watch::default();
        let open = [PEDAL, r"\\?\usb#keys", PEDAL];
        assert_eq!(w.notice(&Wake::Removed(PEDAL.to_lowercase()), &open), [0, 2]);
        assert_eq!(w.notice(&Wake::Removed(r"\\?\usb#other".into()), &open), Vec::<usize>::new());
        assert!(!w.may_open(PEDAL));
        assert!(w.may_open(r"\\?\usb#other"), "a removal that closed nothing holds nothing back");
    }

    // A duplicate notification is harmless: the connection it named is already gone.
    #[test]
    fn a_duplicate_removal_or_arrival_does_nothing_more() {
        let mut w = Watch::default();
        assert_eq!(w.notice(&Wake::Removed(PEDAL.into()), &[PEDAL]), [0]);
        assert_eq!(w.notice(&Wake::Removed(PEDAL.into()), &[]), Vec::<usize>::new());
        assert_eq!(w.removed.len(), 1);
        w.notice(&Wake::Arrived(PEDAL.into()), &[]);
        w.notice(&Wake::Arrived(PEDAL.into()), &[]);
        assert!(w.may_open(PEDAL));
        assert_eq!(w.retry, Some(0), "a second arrival restarts the schedule, never lengthens it");
    }

    // Hole 2: WinMM may still list a port a removal closed; it stays closed until WinMM drops it or the
    // interface arrives again, so a dead listing is never reopened.
    #[test]
    fn a_removed_port_reopens_only_after_it_left_the_list_or_arrived_again() {
        let mut w = Watch::default();
        w.notice(&Wake::Removed(PEDAL.into()), &[PEDAL]);
        w.listed(&[PEDAL]);
        assert!(!w.may_open(PEDAL), "still listed");
        w.listed(&[]);
        assert!(w.may_open(PEDAL), "left the list");

        w.notice(&Wake::Removed(PEDAL.into()), &[PEDAL]);
        w.notice(&Wake::Arrived(PEDAL.into()), &[]);
        assert!(w.may_open(PEDAL), "the replug arrived inside one poll");
    }

    // The bounded retry: an arrival re-enumerates on a doubling schedule, then the poll takes over.
    #[test]
    fn an_arrival_runs_a_bounded_backoff_then_the_poll() {
        let ms = |w: &mut Watch| w.next_wait().as_millis() as u64;
        let mut w = Watch::default();
        assert_eq!(ms(&mut w), POLL.as_millis() as u64);
        w.notice(&Wake::Arrived(PEDAL.into()), &[]);
        let waits: Vec<u64> = (0..7).map(|_| ms(&mut w)).collect();
        assert_eq!(waits, [50, 100, 200, 400, 800, 1000, 1000]);
        assert_eq!(retry_wait(5), None);
        assert!(RETRY_MS.windows(2).all(|p| p[1] == 2 * p[0]));
    }

    // A weak identity has no path: no notification can name it.
    #[test]
    fn an_empty_path_matches_nothing() {
        let mut w = Watch::default();
        assert_eq!(w.notice(&Wake::Removed(String::new()), &[""]), Vec::<usize>::new());
        assert!(!same_path("", ""));
    }

    // Decision 10: a callback from an old connection generation is ignored.
    #[test]
    fn a_callback_from_an_invalidated_generation_is_dropped() {
        let g = Generation::default();
        let ticket = g.current();
        let callback = g.clone();
        assert!(callback.admits(ticket));
        g.invalidate();
        assert!(!callback.admits(ticket));
        g.invalidate();
        assert!(!callback.admits(ticket), "a duplicate invalidation keeps it dropped");
    }
}
