//! Linux plugin editor windows and the owner thread's run loop: the stand-in for `editor_window.rs`
//! (Win32) with the same names, so the shared CLAP and VST3 owners (`clap.rs`, `vst3.rs`,
//! `engine_slot.rs`) differ only at small `cfg` seams.
//!
//! **Windows.** A hosted editor embeds into an X11 top-level of our own (x11rb, pure Rust; under a
//! Wayland desktop it is an XWayland client, as the plugin's own GUI is). Each owner thread opens one X
//! connection with its first editor window and drops it with its last; an `HWND` carries the window's
//! XID as a pointer-sized integer. The window sets `WM_DELETE_WINDOW`: a window manager's close
//! arrives as that ClientMessage and makes `close_requested()` true, which the owner tears down as it
//! does the Win32 close box. Fixed size (min = max size hints), as the Win32 window is; the plugin's
//! own resize requests land in `set_client_size`. The window's own `ConfigureNotify` records the size
//! it really has (`client_size`); **known limit:** a window manager that configures it to another size
//! is logged, but the plugin is not told (no renegotiation). Creation, its properties and its map are
//! checked requests, so an open the X server refused fails. The window is a minimal XEmbed embedder:
//! the plugin's window gets `XEMBED_EMBEDDED_NOTIFY` when it arrives and the top-level's focus as
//! `XEMBED_WINDOW_(DE)ACTIVATE` + `XEMBED_FOCUS_IN`/`OUT` (JUCE waits for these); nothing more of the
//! spec (no accelerators, no tab chain, no key forwarding). A test build maps the window at creation (the CLAP
//! open shows it only outside tests, and an unmapped parent draws nothing); the Linux tests that open
//! one need `DISPLAY`, which only a private Xvfb gives them.
//!
//! **Run loop.** A Linux plugin GUI (JUCE among them) draws nothing unless the host runs its file
//! descriptors and timers on the GUI (owner) thread: VST3 through the `IRunLoop` its `IPlugFrame`
//! answers (`vst3.rs`), CLAP through `timer-support` and `posix-fd-support` (`clap.rs`). Both register
//! here, in the owner thread's own registry (both APIs register on the main thread, which is the
//! owner). `pump_thread_messages` drains our X events and queues the plugin's ready fds and due timers;
//! the format's `idle`, which the owner loop runs right after it, calls the plugin back for each
//! (`next_ready`), so the plugin is only ever called on its owner thread, between turns.
//! `wait_for_input` is one `poll()` over the X connection, the plugin's fds and the nearest timer,
//! capped at what the owner loop asks (20 ms). With no hosted editor the owner loop waits on its
//! request channel instead (as on Windows), so a plugin's timers then fire at its 20 ms turns.
//!
//! What runs here is the owner thread's; invariant 5 is untouched. The one thing another thread may
//! call is the `GetWindowThreadProcessId`/`GetCurrentThreadId` pair (a CLAP `request_resize` from the
//! audio thread): both read const-initialised thread-locals only, which never allocate.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::cookie::VoidCookie;
use x11rb::errors::ConnectionError;
use x11rb::properties::WmSizeHints;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt as _, CreateWindowAux, EventMask, NotifyDetail,
    NotifyMode, PropMode, StackMode, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// A native window handle in the shape the Windows `HWND` has (`.0` is the pointer): on Linux the
/// X11 window id (XID) as a pointer-sized integer, 0 for none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct HWND(pub *mut c_void);

impl HWND {
    pub(super) fn from_xid(xid: u32) -> HWND {
        HWND(xid as usize as *mut c_void)
    }

    /// The XID, `None` for the null handle or a value no XID has (XIDs are 29-bit).
    pub(super) fn xid(self) -> Option<u32> {
        u32::try_from(self.0 as usize).ok().filter(|&xid| xid != 0)
    }
}

/// `WM_CLASS` (instance, class): what a compositor's window rules match.
const WM_CLASS: &[u8] = b"bleeploop-plugin\0BleepLoop\0";
/// The largest side an X window takes (a CARD16 that X treats as signed in places).
const MAX_SIDE: u32 = i16::MAX as u32;

// ---- the owner thread's run-loop registry (pure: no X, no syscalls) -------------------------------

/// A plugin fd's interest and readiness bits: CLAP's `CLAP_POSIX_FD_*` values, which the CLAP host
/// passes through as they are. VST3 registers `FD_READ`.
pub(super) const FD_READ: u32 = 1 << 0;
pub(super) const FD_WRITE: u32 = 1 << 1;
pub(super) const FD_ERROR: u32 = 1 << 2;
/// The shortest timer period run: a 0 ms request would turn the owner loop without a wait.
const MIN_TIMER_PERIOD: Duration = Duration::from_millis(1);

/// Something the plugin registered that is due a callback on the owner thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Ready {
    /// The fd and the bits it is ready for.
    Fd(RawFd, u32),
    /// The timer's id.
    Timer(u32),
}

struct Timer {
    id: u32,
    period: Duration,
    due: Instant,
}

/// The fds and timers the owner thread's plugin registered, and what is due a callback. Each
/// registration is queued at most once until it is taken, so a plugin that is never dispatched cannot
/// grow the queue.
#[derive(Default)]
struct Sources {
    fds: Vec<(RawFd, u32)>,
    timers: Vec<Timer>,
    last_timer: u32,
    ready: VecDeque<Ready>,
}

impl Sources {
    /// False when `fd` is registered already (CLAP: modify it instead).
    fn add_fd(&mut self, fd: RawFd, interest: u32) -> bool {
        if fd < 0 || self.fds.iter().any(|&(f, _)| f == fd) {
            return false;
        }
        self.fds.push((fd, interest));
        true
    }

    fn modify_fd(&mut self, fd: RawFd, interest: u32) -> bool {
        match self.fds.iter_mut().find(|(f, _)| *f == fd) {
            Some(entry) => {
                entry.1 = interest;
                true
            }
            None => false,
        }
    }

    /// Unregister `fd`; a callback queued for it is dropped with it.
    fn remove_fd(&mut self, fd: RawFd) -> bool {
        let before = self.fds.len();
        self.fds.retain(|&(f, _)| f != fd);
        self.ready.retain(|r| !matches!(r, Ready::Fd(f, _) if *f == fd));
        self.fds.len() != before
    }

    /// Register a timer first due one period from `now`. Ids start at 1 and are never reused while
    /// registered; `u32::MAX` (CLAP's invalid id) is never handed out.
    fn add_timer(&mut self, period_ms: u32, now: Instant) -> u32 {
        let period = Duration::from_millis(u64::from(period_ms)).max(MIN_TIMER_PERIOD);
        let mut id = self.last_timer;
        loop {
            id = if id >= u32::MAX - 1 { 1 } else { id + 1 };
            if !self.timers.iter().any(|t| t.id == id) {
                break;
            }
        }
        self.last_timer = id;
        self.timers.push(Timer { id, period, due: now + period });
        id
    }

    /// Unregister timer `id`; a callback queued for it is dropped with it.
    fn remove_timer(&mut self, id: u32) -> bool {
        let before = self.timers.len();
        self.timers.retain(|t| t.id != id);
        self.ready.retain(|r| *r != Ready::Timer(id));
        self.timers.len() != before
    }

    fn has_fd(&self, fd: RawFd) -> bool {
        self.fds.iter().any(|&(f, _)| f == fd)
    }

    fn has_timer(&self, id: u32) -> bool {
        self.timers.iter().any(|t| t.id == id)
    }

    /// How long the owner may wait for input: nothing while a callback is queued, else up to the
    /// nearest timer, at most `cap`.
    fn wait(&self, now: Instant, cap: Duration) -> Duration {
        if !self.ready.is_empty() {
            return Duration::ZERO;
        }
        self.timers.iter().map(|t| t.due.saturating_duration_since(now)).fold(cap, Duration::min)
    }

    /// Queue every timer due at `now`, once however many periods it fell behind, and schedule its next
    /// tick a period on (from `now` when it fell a whole period behind: no burst of catch-up ticks).
    fn queue_due(&mut self, now: Instant) {
        for timer in &mut self.timers {
            if timer.due > now {
                continue;
            }
            timer.due += timer.period;
            if timer.due <= now {
                timer.due = now + timer.period;
            }
            if !self.ready.contains(&Ready::Timer(timer.id)) {
                self.ready.push_back(Ready::Timer(timer.id));
            }
        }
    }

    /// Queue a registered fd that is ready for `bits` (an error is always reported), merged into its
    /// queued entry when it has one.
    fn queue_fd(&mut self, fd: RawFd, bits: u32) {
        let Some(&(_, interest)) = self.fds.iter().find(|&&(f, _)| f == fd) else { return };
        let bits = bits & (interest | FD_ERROR);
        if bits == 0 {
            return;
        }
        for r in &mut self.ready {
            if let Ready::Fd(f, queued) = r {
                if *f == fd {
                    *queued |= bits;
                    return;
                }
            }
        }
        self.ready.push_back(Ready::Fd(fd, bits));
    }

    /// The next queued callback whose registration still stands (an earlier callback may have
    /// unregistered it). An fd's queued bits are cut to its interest as it stands now (an earlier
    /// callback may have modified it), and an fd left with none is dropped.
    fn next_ready(&mut self) -> Option<Ready> {
        while let Some(r) = self.ready.pop_front() {
            match r {
                Ready::Fd(fd, bits) => {
                    let Some(&(_, interest)) = self.fds.iter().find(|&&(f, _)| f == fd) else { continue };
                    let bits = bits & (interest | FD_ERROR);
                    if bits != 0 {
                        return Some(Ready::Fd(fd, bits));
                    }
                }
                Ready::Timer(id) if self.has_timer(id) => return Some(r),
                Ready::Timer(_) => {}
            }
        }
        None
    }
}

/// `poll()` bits for a plugin fd's interest.
fn poll_events(interest: u32) -> libc::c_short {
    let mut events = 0;
    if interest & FD_READ != 0 {
        events |= libc::POLLIN | libc::POLLPRI;
    }
    if interest & FD_WRITE != 0 {
        events |= libc::POLLOUT;
    }
    events
}

/// A `poll()` result as plugin fd bits.
fn ready_bits(revents: libc::c_short) -> u32 {
    let mut bits = 0;
    if revents & (libc::POLLIN | libc::POLLPRI) != 0 {
        bits |= FD_READ;
    }
    if revents & libc::POLLOUT != 0 {
        bits |= FD_WRITE;
    }
    if revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
        bits |= FD_ERROR;
    }
    bits
}

/// A wait as `poll()`'s millisecond timeout, rounded up so a timer is never woken for early.
fn poll_timeout_ms(wait: Duration) -> libc::c_int {
    libc::c_int::try_from(wait.as_micros().div_ceil(1000)).unwrap_or(libc::c_int::MAX)
}

/// `poll()` over `fds` for up to `timeout_ms`, retried on EINTR. Errors leave `revents` at zero.
fn poll(fds: &mut [libc::pollfd], timeout_ms: libc::c_int) {
    for pfd in fds.iter_mut() {
        pfd.revents = 0;
    }
    loop {
        // SAFETY: `fds` is a valid, exclusively borrowed array of `fds.len()` pollfd entries.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
        if n >= 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return;
        }
    }
}

// ---- the owner thread's X connection ---------------------------------------------------------------

/// XEmbed messages and the details this embedder sends (XEmbed spec 0.5).
const XEMBED_EMBEDDED_NOTIFY: u32 = 0;
const XEMBED_WINDOW_ACTIVATE: u32 = 1;
const XEMBED_WINDOW_DEACTIVATE: u32 = 2;
const XEMBED_FOCUS_IN: u32 = 4;
const XEMBED_FOCUS_OUT: u32 = 5;
const XEMBED_FOCUS_CURRENT: u32 = 0;

/// One editor window as this thread knows it. Pure bookkeeping: the X calls are `Display`'s.
struct Window {
    xid: u32,
    /// The size the window has: what this host asked, until a configure says otherwise.
    size: (u32, u32),
    /// The size this host last asked for.
    asked: (u32, u32),
    /// The sequence number of the last resize request: a configure the server sent before it
    /// processed that request is stale.
    asked_seq: u64,
    closed: bool,
    /// The plugin's window embedded in this one (its first child), once it arrived.
    child: Option<u32>,
    /// The top-level has the focus (the XEmbed "window active" state).
    active: bool,
}

/// What an embedded child is told about its embedder's focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Activation {
    Active,
    Inactive,
}

impl Window {
    fn new(xid: u32, size: (u32, u32)) -> Window {
        Window { xid, size, asked: size, asked_seq: 0, closed: false, child: None, active: false }
    }

    /// A child was created in or reparented into this window: the one to embed, when it is the
    /// first.
    fn child_arrived(&mut self, child: u32) -> Option<u32> {
        if self.child.is_some() || child == self.xid {
            return None;
        }
        self.child = Some(child);
        Some(child)
    }

    /// A child was destroyed or reparented away.
    fn child_left(&mut self, child: u32) {
        if self.child == Some(child) {
            self.child = None;
        }
    }

    /// The top-level's `FocusIn`/`FocusOut`: what to tell the child, when the window's active state
    /// changed. A grab's focus round trip and focus moving to or from the child itself (an inferior)
    /// change nothing.
    fn focus(&mut self, focus_in: bool, detail: NotifyDetail, mode: NotifyMode) -> Option<(u32, Activation)> {
        if matches!(mode, NotifyMode::GRAB | NotifyMode::UNGRAB) || matches!(detail, NotifyDetail::INFERIOR | NotifyDetail::POINTER) {
            return None;
        }
        if self.active == focus_in {
            return None;
        }
        self.active = focus_in;
        Some((self.child?, if focus_in { Activation::Active } else { Activation::Inactive }))
    }

    /// The window's own `ConfigureNotify` (sequence `seq`): the size it has now, unless the server
    /// sent it before it took the last resize. True when that size is not the one asked.
    fn configured(&mut self, size: (u32, u32), seq: u64) -> bool {
        if seq < self.asked_seq {
            return false;
        }
        self.size = size;
        size != self.asked
    }
}

struct Display {
    conn: RustConnection,
    root: u32,
    black: u32,
    wm_protocols: u32,
    wm_delete: u32,
    net_wm_name: u32,
    utf8_string: u32,
    xembed: u32,
    windows: Vec<Window>,
}

type XResult<T> = Result<T, Box<dyn std::error::Error>>;

impl Display {
    fn open() -> Result<Display, String> {
        let (conn, screen) = x11rb::connect(None).map_err(|e| format!("no X display for the plugin editor: {e}"))?;
        Display::with(conn, screen).map_err(|e| format!("X display setup for the plugin editor: {e}"))
    }

    fn with(conn: RustConnection, screen: usize) -> XResult<Display> {
        let (root, black) = {
            let s = conn.setup().roots.get(screen).ok_or("the X display has no such screen")?;
            (s.root, s.black_pixel)
        };
        let cookies = [b"WM_PROTOCOLS".as_slice(), b"WM_DELETE_WINDOW", b"_NET_WM_NAME", b"UTF8_STRING", b"_XEMBED"]
            .map(|name| conn.intern_atom(false, name));
        let mut atoms = [0u32; 5];
        for (atom, cookie) in atoms.iter_mut().zip(cookies) {
            *atom = cookie?.reply()?.atom;
        }
        let [wm_protocols, wm_delete, net_wm_name, utf8_string, xembed] = atoms;
        Ok(Display { conn, root, black, wm_protocols, wm_delete, net_wm_name, utf8_string, xembed, windows: Vec::new() })
    }

    fn window(&mut self, xid: u32) -> Option<&mut Window> {
        self.windows.iter_mut().find(|w| w.xid == xid)
    }

    /// Create the window, its properties and (in a test build) its map as checked requests: an open
    /// the X server refused is an error, never a success with no window. A failed step destroys what
    /// was created.
    fn create(&mut self, width: u32, height: u32, title: &str) -> XResult<u32> {
        let (w, h) = (side(width), side(height));
        let xid = self.conn.generate_id()?;
        // Structure: its own close, configure and destroy; substructure: the plugin's window arriving
        // in it (XEmbed); focus: the XEmbed activation the child is told about.
        let events = EventMask::STRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_NOTIFY | EventMask::FOCUS_CHANGE;
        let aux = CreateWindowAux::new().background_pixel(self.black).event_mask(events);
        self.conn
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                xid,
                self.root,
                0,
                0,
                w as u16,
                h as u16,
                0,
                WindowClass::INPUT_OUTPUT,
                x11rb::COPY_FROM_PARENT,
                &aux,
            )?
            .check()?;
        let set_up = || -> XResult<()> {
            let conn = &self.conn;
            conn.change_property32(PropMode::REPLACE, xid, self.wm_protocols, AtomEnum::ATOM, &[self.wm_delete])?.check()?;
            conn.change_property8(PropMode::REPLACE, xid, AtomEnum::WM_NAME, AtomEnum::STRING, title.as_bytes())?.check()?;
            conn.change_property8(PropMode::REPLACE, xid, self.net_wm_name, self.utf8_string, title.as_bytes())?.check()?;
            conn.change_property8(PropMode::REPLACE, xid, AtomEnum::WM_CLASS, AtomEnum::STRING, WM_CLASS)?.check()?;
            fixed_size(conn, xid, w, h)?.check()?;
            if cfg!(test) {
                conn.map_window(xid)?.check()?;
            }
            Ok(())
        };
        // Checked: the server has created the window before the plugin, on a connection of its own,
        // is handed its id.
        if let Err(e) = set_up() {
            let _ = self.conn.destroy_window(xid);
            let _ = self.conn.flush();
            return Err(e);
        }
        self.windows.push(Window::new(xid, (w, h)));
        Ok(xid)
    }

    fn resize(&mut self, xid: u32, width: u32, height: u32) -> XResult<(u32, u32)> {
        let (w, h) = (side(width), side(height));
        // Hints first: a window manager holds a configure against the min/max size it last read.
        drop(fixed_size(&self.conn, xid, w, h)?);
        let seq = self.conn.configure_window(xid, &ConfigureWindowAux::new().width(w).height(h))?.sequence_number();
        self.conn.flush()?;
        if let Some(win) = self.window(xid) {
            win.size = (w, h);
            win.asked = (w, h);
            win.asked_seq = seq;
        }
        Ok((w, h))
    }

    /// Map (checked) and raise.
    fn show(&self, xid: u32) -> XResult<()> {
        self.conn.map_window(xid)?.check()?;
        self.conn.configure_window(xid, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE))?;
        self.conn.flush()?;
        Ok(())
    }

    fn destroy(&mut self, xid: u32) {
        self.windows.retain(|w| w.xid != xid);
        let _ = self.conn.destroy_window(xid);
        let _ = self.conn.flush();
    }

    /// An `_XEMBED` message to the embedded child, as the spec sends it (no event mask).
    fn xembed(&self, child: u32, message: u32, detail: u32, data1: u32) {
        let event = ClientMessageEvent::new(32, child, self.xembed, [x11rb::CURRENT_TIME, message, detail, data1, 0]);
        let _ = self.conn.send_event(false, child, EventMask::NO_EVENT, event);
    }

    /// Tell the child its embedder became active (with the focus on the child) or inactive.
    fn activate(&self, child: u32, activation: Activation) {
        match activation {
            Activation::Active => {
                self.xembed(child, XEMBED_WINDOW_ACTIVATE, 0, 0);
                self.xembed(child, XEMBED_FOCUS_IN, XEMBED_FOCUS_CURRENT, 0);
            }
            Activation::Inactive => {
                self.xembed(child, XEMBED_WINDOW_DEACTIVATE, 0, 0);
                self.xembed(child, XEMBED_FOCUS_OUT, 0, 0);
            }
        }
    }

    /// A child arrived: `XEMBED_EMBEDDED_NOTIFY` (data1: the embedder, data2: protocol version 0),
    /// then its activation when the window is active already. The minimal embedder JUCE and other
    /// toolkits expect; no accelerators, no tab chain, and no `XSetInputFocus` (the spec keeps the X
    /// focus on the embedder: a plugin that wants keys takes the focus itself on a click).
    fn embed(&mut self, host: u32, child: u32) {
        let Some(win) = self.window(host) else { return };
        let Some(child) = win.child_arrived(child) else { return };
        let active = win.active;
        self.xembed(child, XEMBED_EMBEDDED_NOTIFY, 0, host);
        if active {
            self.activate(child, Activation::Active);
        }
    }

    /// Take every queued X event: a window manager's `WM_DELETE_WINDOW` marks its window closed and
    /// unmaps it (instant feedback; the owner destroys it after the plugin let go); the plugin's
    /// window arriving and the window's focus drive XEmbed; a configure records the size the window
    /// has. A broken connection closes every window, so each editor tears down and the next open
    /// reconnects.
    fn drain(&mut self) {
        loop {
            let (event, seq) = match self.conn.poll_for_event_with_sequence() {
                Ok(Some(next)) => next,
                Ok(None) => break,
                Err(e) => {
                    log::error!("[plugin_host] the editor's X connection broke ({e}); closing its windows");
                    for win in &mut self.windows {
                        win.closed = true;
                    }
                    return;
                }
            };
            match event {
                Event::ClientMessage(ev)
                    if ev.type_ == self.wm_protocols && ev.format == 32 && ev.data.as_data32()[0] == self.wm_delete =>
                {
                    if let Some(win) = self.window(ev.window) {
                        win.closed = true;
                        let _ = self.conn.unmap_window(ev.window);
                    }
                }
                Event::CreateNotify(ev) => self.embed(ev.parent, ev.window),
                Event::ReparentNotify(ev) if ev.event != ev.window => {
                    if ev.parent == ev.event {
                        self.embed(ev.event, ev.window);
                    } else if let Some(win) = self.window(ev.event) {
                        win.child_left(ev.window);
                    }
                }
                Event::DestroyNotify(ev) if ev.event != ev.window => {
                    if let Some(win) = self.window(ev.event) {
                        win.child_left(ev.window);
                    }
                }
                Event::FocusIn(ev) => self.focus(ev.event, true, ev.detail, ev.mode),
                Event::FocusOut(ev) => self.focus(ev.event, false, ev.detail, ev.mode),
                Event::ConfigureNotify(ev) if ev.event == ev.window => {
                    let size = (u32::from(ev.width), u32::from(ev.height));
                    if let Some(win) = self.window(ev.window) {
                        if win.configured(size, seq) {
                            log::info!(
                                "[plugin_host] the editor window is {}x{}, not the {}x{} asked (the plugin keeps its own size)",
                                size.0,
                                size.1,
                                win.asked.0,
                                win.asked.1
                            );
                        }
                    }
                }
                _ => {}
            }
        }
        let _ = self.conn.flush();
    }

    fn focus(&mut self, xid: u32, focus_in: bool, detail: NotifyDetail, mode: NotifyMode) {
        let Some(win) = self.window(xid) else { return };
        if let Some((child, activation)) = win.focus(focus_in, detail, mode) {
            self.activate(child, activation);
        }
    }
}

/// A requested side as an X window takes it: at least 1, at most `MAX_SIDE`.
fn side(pixels: u32) -> u32 {
    pixels.clamp(1, MAX_SIDE)
}

/// Pin the window's size for a window manager (min = max), as the Win32 window has no sizing frame.
fn fixed_size(conn: &RustConnection, xid: u32, w: u32, h: u32) -> Result<VoidCookie<'_, RustConnection>, ConnectionError> {
    let hints = WmSizeHints { min_size: Some((w as i32, h as i32)), max_size: Some((w as i32, h as i32)), ..WmSizeHints::new() };
    hints.set_normal_hints(conn, xid)
}

#[derive(Default)]
struct Owner {
    display: Option<Display>,
    title: String,
    sources: Sources,
}

impl Owner {
    /// Drop the connection once its last window is gone.
    fn release_idle_display(&mut self) {
        if self.display.as_ref().is_some_and(|d| d.windows.is_empty()) {
            self.display = None;
        }
    }

    /// The `poll()` set: the X connection (when open) and the plugin's fds, in that order.
    fn pollfds(&self) -> Vec<libc::pollfd> {
        let x = self.display.as_ref().map(|d| libc::pollfd { fd: d.conn.stream().as_raw_fd(), events: libc::POLLIN, revents: 0 });
        let plugin = self.sources.fds.iter().map(|&(fd, interest)| libc::pollfd { fd, events: poll_events(interest), revents: 0 });
        x.into_iter().chain(plugin).collect()
    }

    /// Queue the plugin fds a `poll()` found ready (`fds` from `pollfds`). An fd the kernel calls
    /// invalid is unregistered: left in, it would wake every wait at once.
    fn queue_polled(&mut self, fds: &[libc::pollfd]) {
        let skip = usize::from(self.display.is_some());
        for pfd in &fds[skip..] {
            if pfd.revents & libc::POLLNVAL != 0 {
                log::warn!("[plugin_host] the plugin registered fd {} and closed it; unregistered", pfd.fd);
                self.sources.remove_fd(pfd.fd);
            } else if pfd.revents != 0 {
                self.sources.queue_fd(pfd.fd, ready_bits(pfd.revents));
            }
        }
    }
}

thread_local! {
    static OWNER: RefCell<Owner> = RefCell::new(Owner::default());
    /// The XID of the editor window this thread created last and still holds (0 = none). Const and
    /// drop-free: any thread may read it without allocating (`GetWindowThreadProcessId`).
    static OWN_WINDOW: Cell<u32> = const { Cell::new(0) };
    /// This thread's id for the `GetCurrentThreadId` shim (0 = not assigned yet).
    static THREAD_TOKEN: Cell<u32> = const { Cell::new(0) };
}
static NEXT_THREAD_TOKEN: AtomicU32 = AtomicU32::new(1);

fn with_owner<T>(f: impl FnOnce(&mut Owner) -> T) -> Option<T> {
    OWNER.try_with(|owner| f(&mut owner.borrow_mut())).ok()
}

/// The title this owner thread's editor windows get: the plugin's name (the owner sets it at load).
pub(super) fn set_editor_title(name: &str) {
    with_owner(|o| o.title = name.to_string());
}

/// A host-owned top-level editor window (RAII: dropping it destroys the X window, and the thread's
/// X connection with its last window).
pub(super) struct HostWindow {
    pub(super) hwnd: HWND,
}

impl HostWindow {
    /// A window manager asked to close the window (its close button), or the X connection broke.
    pub(super) fn close_requested(&self) -> bool {
        let Some(xid) = self.hwnd.xid() else { return false };
        with_owner(|o| o.display.as_mut().and_then(|d| d.window(xid)).is_none_or(|w| w.closed)).unwrap_or(true)
    }
}

impl Drop for HostWindow {
    fn drop(&mut self) {
        let Some(xid) = self.hwnd.xid() else { return };
        let _ = OWN_WINDOW.try_with(|own| {
            if own.get() == xid {
                own.set(0);
            }
        });
        with_owner(|o| {
            if let Some(display) = o.display.as_mut() {
                display.destroy(xid);
            }
            o.release_idle_display();
        });
    }
}

/// Create the host-owned editor window with a client area of `width`×`height` (the plugin's size),
/// titled with the plugin's name, unmapped until `show_host_window_front` (mapped at once in a test
/// build: module doc). Opens the thread's X connection (`DISPLAY`) with its first window.
pub(super) fn create_host_window(width: u32, height: u32) -> Result<HostWindow, String> {
    let created = with_owner(|o| {
        if o.display.is_none() {
            o.display = Some(Display::open()?);
        }
        let title = if o.title.is_empty() { "BleepLoop Plugin".to_string() } else { o.title.clone() };
        let display = o.display.as_mut().expect("opened above");
        let created = display.create(width, height, &title).map_err(|e| format!("X editor window: {e}"));
        o.release_idle_display();
        created
    })
    .ok_or_else(|| "the owner thread is exiting".to_string())??;
    let _ = OWN_WINDOW.try_with(|own| own.set(created));
    // The XID, for a probe on the display to find the window (and check it is gone after the close).
    log::info!("[plugin_host] editor window {created:#x} open");
    Ok(HostWindow { hwnd: HWND::from_xid(created) })
}

/// The window's client size in pixels (0×0 for a window it does not hold): the size the last
/// configure the owner's pump took says it has, else the size asked. Under a window manager a
/// configure lands asynchronously, so right after a resize this is the size asked.
pub(super) fn client_size(hwnd: HWND) -> (u32, u32) {
    let Some(xid) = hwnd.xid() else { return (0, 0) };
    with_owner(|o| o.display.as_mut().and_then(|d| d.window(xid)).map(|w| w.size))
        .flatten()
        .unwrap_or((0, 0))
}

/// Resize the window's client area to `width`×`height`: the one place a plugin-initiated resize
/// lands, as on Windows. A side past what an X window takes is clamped, and the size it got is the
/// answer (CLAP and VST3 tell the plugin that size). `None` for a window this thread does not hold.
pub(super) fn set_client_size(hwnd: HWND, width: u32, height: u32) -> Option<(u32, u32)> {
    let xid = hwnd.xid()?;
    with_owner(|o| {
        let display = o.display.as_mut()?;
        display.window(xid)?;
        display.resize(xid, width, height).inspect_err(|e| log::warn!("[plugin_host] editor resize: {e}")).ok()
    })
    .flatten()
}

/// Map the editor window and raise it, once, at open. Whether it takes the focus is the window
/// manager's call.
pub(super) fn show_host_window_front(hwnd: HWND) {
    let Some(xid) = hwnd.xid() else { return };
    with_owner(|o| {
        if let Some(display) = o.display.as_ref() {
            if let Err(e) = display.show(xid) {
                log::warn!("[plugin_host] showing the editor window: {e}");
            }
        }
    });
}

/// The owner's turn: take our X events (a close request), and queue the plugin's ready fds and due
/// timers for its format's `idle` to call back (`next_ready`). Never blocks.
pub(super) fn pump_thread_messages() {
    with_owner(|o| {
        if let Some(display) = o.display.as_mut() {
            display.drain();
        }
        if !o.sources.fds.is_empty() {
            let mut fds = o.pollfds();
            poll(&mut fds, 0);
            o.queue_polled(&fds);
        }
        o.sources.queue_due(Instant::now());
    });
}

/// Block up to `ms` for input: our X connection, a plugin fd, or the plugin's nearest timer, whichever
/// comes first (at once when a callback is still queued). What woke it is taken by the next turn's
/// `pump_thread_messages`. An X event x11rb already read off the socket during a reply waits out the
/// cap: bounded at `ms`.
pub(super) fn wait_for_input(ms: u32) {
    let planned = with_owner(|o| (o.pollfds(), o.sources.wait(Instant::now(), Duration::from_millis(u64::from(ms)))));
    let Some((mut fds, wait)) = planned else { return };
    if !wait.is_zero() {
        poll(&mut fds, poll_timeout_ms(wait));
    }
}

/// Nothing to drain: an X window has no cross-thread message traffic to answer, and the plugin's own
/// teardown runs on its own connection.
pub(super) fn drain_after_editor_teardown() {}

/// The next plugin callback due on this owner thread, for the format's `idle` to make (module doc).
pub(super) fn next_ready() -> Option<Ready> {
    with_owner(|o| o.sources.next_ready()).flatten()
}

/// Register a plugin fd for `interest` (`FD_*` bits). False when it is registered already.
pub(super) fn register_fd(fd: RawFd, interest: u32) -> bool {
    with_owner(|o| o.sources.add_fd(fd, interest)).unwrap_or(false)
}

pub(super) fn modify_fd(fd: RawFd, interest: u32) -> bool {
    with_owner(|o| o.sources.modify_fd(fd, interest)).unwrap_or(false)
}

pub(super) fn unregister_fd(fd: RawFd) -> bool {
    with_owner(|o| o.sources.remove_fd(fd)).unwrap_or(false)
}

/// Register a plugin timer of `period_ms`; its id.
pub(super) fn register_timer(period_ms: u32) -> u32 {
    with_owner(|o| o.sources.add_timer(period_ms, Instant::now())).unwrap_or(u32::MAX)
}

pub(super) fn unregister_timer(id: u32) -> bool {
    with_owner(|o| o.sources.remove_timer(id)).unwrap_or(false)
}

/// How many fds and timers this thread's registry holds.
#[cfg(test)]
pub(super) fn registered() -> (usize, usize) {
    with_owner(|o| (o.sources.fds.len(), o.sources.timers.len())).unwrap_or_default()
}

/// Drop every fd and timer registered on this thread, with what is queued for them: the plugin is
/// gone and must never be called back.
pub(super) fn forget_registrations() {
    with_owner(|o| o.sources = Sources::default());
}

/// The thread a window belongs to, as the Windows call answers it: this thread's id for the window
/// this thread holds, else 0 (no thread), so a CLAP `request_resize` off the window's thread is
/// queued for the owner's turn. Reads const thread-locals only (safe on the audio thread).
#[allow(non_snake_case)]
pub(super) unsafe fn GetWindowThreadProcessId(hwnd: HWND, _pid: Option<*mut u32>) -> u32 {
    let owned = hwnd.xid().is_some_and(|xid| OWN_WINDOW.try_with(Cell::get).is_ok_and(|own| own == xid));
    if owned {
        GetCurrentThreadId()
    } else {
        0
    }
}

/// The owner thread of a plugin's host object (the CLAP host's main-thread data, the VST3 frame):
/// the thread that created it. The registry is per thread, so a registration must come from that
/// thread: a JUCE module shares one message loop across its instances and can call instance B's
/// host from instance A's owner. The editor-affinity rule (`src/ui/state/instrument.ts`
/// `editorAffinityBlocker`) keeps that from happening; this refuses what gets past it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct OwnerThread(u32);

impl Default for OwnerThread {
    /// The calling thread.
    fn default() -> Self {
        // SAFETY: the shim only reads and sets this thread's own token.
        OwnerThread(unsafe { GetCurrentThreadId() })
    }
}

impl OwnerThread {
    /// Whether the calling thread is the owner; a call from any other is logged once here, and the
    /// caller refuses it without touching any state.
    pub(super) fn admits(self, what: &str) -> bool {
        if self == OwnerThread::default() {
            return true;
        }
        log::warn!("[plugin_host] the plugin called {what} off its owner thread; refused");
        false
    }
}

/// A nonzero id for the calling thread, assigned on its first call.
#[allow(non_snake_case)]
pub(super) unsafe fn GetCurrentThreadId() -> u32 {
    THREAD_TOKEN
        .try_with(|token| {
            if token.get() == 0 {
                token.set(NEXT_THREAD_TOKEN.fetch_add(1, Relaxed).max(1));
            }
            token.get()
        })
        .unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn an_xid_round_trips_through_the_handle() {
        for xid in [1, 0x0060_0001, 0x1fff_ffff] {
            assert_eq!(HWND::from_xid(xid).xid(), Some(xid));
        }
        assert_eq!(HWND::from_xid(0).xid(), None, "the null handle is no window");
        assert_eq!(HWND(std::ptr::null_mut()).xid(), None);
        assert_eq!(HWND((1usize << 40) as *mut c_void).xid(), None, "no XID is that wide");
    }

    /// The wait is the nearest timer's, capped, and nothing while a callback is still queued; a due
    /// timer is queued once and its next tick scheduled a period on, never as a burst of catch-ups.
    #[test]
    fn timers_set_the_wait_and_tick_once_per_turn() {
        let t0 = Instant::now();
        let mut s = Sources::default();
        assert_eq!(s.wait(t0, ms(20)), ms(20), "nothing registered: the cap");
        let slow = s.add_timer(50, t0);
        let fast = s.add_timer(8, t0);
        assert_ne!(slow, fast);
        assert_eq!(s.wait(t0, ms(20)), ms(8), "the nearest timer");
        assert_eq!(s.wait(t0 + ms(3), ms(20)), ms(5));
        assert_eq!(s.wait(t0 + ms(3), ms(2)), ms(2), "capped");

        s.queue_due(t0 + ms(7));
        assert_eq!(s.next_ready(), None, "not due yet");
        s.queue_due(t0 + ms(8));
        assert_eq!(s.wait(t0 + ms(8), ms(20)), Duration::ZERO, "a callback is queued");
        s.queue_due(t0 + ms(9));
        assert_eq!(s.next_ready(), Some(Ready::Timer(fast)), "queued once");
        assert_eq!(s.next_ready(), None);
        assert_eq!(s.wait(t0 + ms(9), ms(20)), ms(7), "next tick a period after the last");

        // Late by five periods: one tick, the next a period from now.
        s.queue_due(t0 + ms(56));
        assert_eq!(s.next_ready(), Some(Ready::Timer(slow)));
        assert_eq!(s.next_ready(), Some(Ready::Timer(fast)));
        assert_eq!(s.next_ready(), None);
        assert_eq!(s.wait(t0 + ms(56), ms(20)), ms(8));

        let zero = s.add_timer(0, t0 + ms(56));
        assert_eq!(s.wait(t0 + ms(56), ms(20)), MIN_TIMER_PERIOD, "a 0 ms timer still waits");
        assert!(s.remove_timer(zero));
        assert!(!s.remove_timer(zero), "gone");
    }

    /// A callback queued for a timer or fd that an earlier callback unregistered is never made.
    #[test]
    fn an_unregistered_source_is_never_called_back() {
        let t0 = Instant::now();
        let mut s = Sources::default();
        let a = s.add_timer(1, t0);
        let b = s.add_timer(1, t0);
        assert!(s.add_fd(7, FD_READ));
        s.queue_due(t0 + ms(1));
        s.queue_fd(7, FD_READ);
        assert_eq!(s.next_ready(), Some(Ready::Timer(a)));
        // a's callback unregisters b and the fd.
        assert!(s.remove_timer(b));
        assert!(s.remove_fd(7));
        assert_eq!(s.next_ready(), None);
    }

    #[test]
    fn the_fd_registry_adds_modifies_and_removes() {
        let mut s = Sources::default();
        assert!(s.add_fd(5, FD_READ));
        assert!(!s.add_fd(5, FD_WRITE), "registered twice");
        assert!(!s.add_fd(-1, FD_READ), "no fd");
        s.queue_fd(5, FD_WRITE);
        assert_eq!(s.next_ready(), None, "not what it waits for");
        assert!(s.modify_fd(5, FD_READ | FD_WRITE));
        s.queue_fd(5, FD_WRITE);
        s.queue_fd(5, FD_READ);
        assert_eq!(s.next_ready(), Some(Ready::Fd(5, FD_READ | FD_WRITE)), "merged into one callback");
        s.queue_fd(5, FD_ERROR);
        assert_eq!(s.next_ready(), Some(Ready::Fd(5, FD_ERROR)), "an error is always reported");
        s.queue_fd(6, FD_READ);
        assert_eq!(s.next_ready(), None, "an fd nobody registered");
        assert!(!s.modify_fd(6, FD_READ));
        assert!(s.remove_fd(5));
        assert!(!s.remove_fd(5));
        assert_eq!(s.wait(Instant::now(), ms(20)), ms(20));
    }

    /// XEmbed bookkeeping: the first child is the one embedded (once), the top-level's focus is told
    /// to it only when the active state changes, and a grab or a focus move into the child itself is
    /// no change.
    #[test]
    fn the_embedder_embeds_its_first_child_and_follows_the_top_level_focus() {
        let (n, m) = (NotifyDetail::NONLINEAR, NotifyMode::NORMAL);
        let mut w = Window::new(10, (400, 300));
        assert_eq!(w.focus(true, n, m), None, "active, but no child to tell yet");
        assert_eq!(w.child_arrived(10), None, "never itself");
        assert_eq!(w.child_arrived(11), Some(11));
        assert_eq!(w.child_arrived(11), None, "embedded once");
        assert_eq!(w.child_arrived(12), None, "one client");
        assert_eq!(w.focus(true, n, m), None, "already active");
        assert_eq!(w.focus(false, NotifyDetail::INFERIOR, m), None, "the focus went into the child");
        assert_eq!(w.focus(false, n, NotifyMode::GRAB), None, "a grab");
        assert_eq!(w.focus(false, n, m), Some((11, Activation::Inactive)));
        assert_eq!(w.focus(true, n, m), Some((11, Activation::Active)));
        w.child_left(12);
        assert_eq!(w.child, Some(11), "not the embedded child");
        w.child_left(11);
        assert_eq!(w.child_arrived(12), Some(12), "the next child is embedded");
    }

    /// A configure records the size the window has; one the server sent before it took the last
    /// resize is stale; a size other than the one asked is reported.
    #[test]
    fn a_configure_records_the_real_size_unless_it_is_stale() {
        let mut w = Window::new(10, (400, 300));
        assert!(!w.configured((400, 300), 5), "the size asked");
        (w.asked, w.asked_seq) = ((800, 600), 9);
        assert!(!w.configured((400, 300), 8), "sent before the resize: stale");
        assert_eq!(w.size, (400, 300));
        assert!(w.configured((640, 480), 9), "the window manager gave another size");
        assert_eq!(w.size, (640, 480), "and that is the size it has");
        assert!(!w.configured((800, 600), 12));
        assert_eq!(w.size, (800, 600));
    }

    /// Readiness queued before a callback modified an fd's interest is cut to the new interest when
    /// it is taken: a dropped WRITE interest is never reported, an error always is.
    #[test]
    fn queued_readiness_follows_a_modified_interest() {
        let mut s = Sources::default();
        assert!(s.add_fd(5, FD_READ | FD_WRITE));
        assert!(s.add_fd(6, FD_WRITE));
        s.queue_fd(5, FD_READ | FD_WRITE);
        s.queue_fd(6, FD_WRITE);
        // An earlier callback drops both fds' WRITE interest after the poll.
        assert!(s.modify_fd(5, FD_READ));
        assert!(s.modify_fd(6, 0));
        assert_eq!(s.next_ready(), Some(Ready::Fd(5, FD_READ)), "WRITE no longer asked for");
        assert_eq!(s.next_ready(), None, "fd 6 asks for nothing: no callback");
        s.queue_fd(6, FD_ERROR);
        assert_eq!(s.next_ready(), Some(Ready::Fd(6, FD_ERROR)), "an error is still reported");
    }

    /// The pieces `wait_for_input` and `pump_thread_messages` put together, over a real pipe: a
    /// registered fd wakes the wait and is queued for its plugin.
    #[test]
    fn a_ready_pipe_wakes_the_wait_and_is_queued() {
        let mut pipe = [0; 2];
        // SAFETY: a valid two-int array for pipe(2).
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
        let [read, write] = pipe;
        let mut o = Owner::default();
        assert!(o.sources.add_fd(read, FD_READ));
        let mut fds = o.pollfds();
        let started = Instant::now();
        poll(&mut fds, 30);
        assert!(started.elapsed() >= ms(25), "nothing to read: the wait runs out");
        o.queue_polled(&fds);
        assert_eq!(o.sources.next_ready(), None);
        // SAFETY: writing one byte from a valid buffer to the pipe's write end.
        assert_eq!(unsafe { libc::write(write, b"x".as_ptr().cast(), 1) }, 1);
        let mut fds = o.pollfds();
        poll(&mut fds, 1000);
        o.queue_polled(&fds);
        assert_eq!(o.sources.next_ready(), Some(Ready::Fd(read, FD_READ)));
        // SAFETY: closing the fds this test opened.
        unsafe {
            libc::close(read);
            libc::close(write);
        }
        let mut fds = o.pollfds();
        poll(&mut fds, 0);
        o.queue_polled(&fds);
        assert!(!o.sources.has_fd(read), "a closed fd is unregistered, not polled again");
    }

    #[test]
    fn a_poll_timeout_rounds_up_to_whole_milliseconds() {
        assert_eq!(poll_timeout_ms(Duration::ZERO), 0);
        assert_eq!(poll_timeout_ms(Duration::from_micros(1)), 1);
        assert_eq!(poll_timeout_ms(ms(20)), 20);
        assert_eq!(poll_timeout_ms(Duration::from_secs(u64::MAX)), libc::c_int::MAX);
    }

    /// Only the thread that holds a window answers as its thread, and no thread holds a window it
    /// never made.
    #[test]
    fn a_window_belongs_to_the_thread_that_holds_it() {
        let me = unsafe { GetCurrentThreadId() };
        assert_ne!(me, 0);
        assert_eq!(unsafe { GetCurrentThreadId() }, me, "stable");
        let other = std::thread::spawn(|| unsafe { GetCurrentThreadId() }).join().unwrap();
        assert_ne!(other, me);
        let hwnd = HWND::from_xid(0x0040_0001);
        assert_eq!(unsafe { GetWindowThreadProcessId(hwnd, None) }, 0, "not held");
        OWN_WINDOW.with(|own| own.set(0x0040_0001));
        assert_eq!(unsafe { GetWindowThreadProcessId(hwnd, None) }, me);
        let elsewhere = std::thread::spawn(|| unsafe { GetWindowThreadProcessId(HWND::from_xid(0x0040_0001), None) }).join().unwrap();
        assert_eq!(elsewhere, 0, "another thread does not hold it");
        OWN_WINDOW.with(|own| own.set(0));
    }
}
