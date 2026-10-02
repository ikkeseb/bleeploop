//! The native CLAP host's shared plugin plumbing (Windows-only — clack + MMCSS): the host callbacks
//! (`LfHost`), the event and request types, the editor, parameter and channel queries the engine's
//! CLAP owner (`clap_engine.rs`) runs on. The gotchas are in `src-tauri/AGENTS.md`. Also hosts the
//! VST3 second format (`vst3_host`, `vst3.rs`) and the engine slots (`engine_slot.rs`) as child
//! modules; both reach these items through `super::`.

use super::editor_window::{
    create_host_window, drain_after_editor_teardown, pump_thread_messages, set_client_size,
    show_host_window_front, wait_for_input, HostWindow,
};
use super::state::ParamDesc;

use std::ffi::CString;
use std::sync::atomic::{
    AtomicBool, AtomicIsize, AtomicU32, AtomicU64,
    Ordering::{Acquire, Relaxed, Release},
};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clack_extensions::audio_ports::{AudioPortInfoBuffer, PluginAudioPorts};
use clack_extensions::params::{
    HostParams, HostParamsImplMainThread, HostParamsImplShared, ParamClearFlags, ParamInfoBuffer,
    ParamRescanFlags, PluginParams,
};
use clack_extensions::gui::{
    GuiApiType, GuiConfiguration, GuiSize, HostGui, HostGuiImpl, PluginGui, Window,
};
use clack_extensions::state::{HostState, HostStateImpl, PluginState};
use clack_host::entry::PluginEntry;
use clack_host::host::{HostError, HostExtensions};
use clack_host::events::event_types::{NoteOffEvent, NoteOnEvent, ParamValueEvent};
use clack_host::events::io::EventBuffer;
use clack_host::events::Match;
use clack_host::prelude::*;
use clack_host::utils::{ClapId, Cookie};

use rtrb::Consumer;

use windows::core::w;
use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::System::Threading::{AvSetMmThreadCharacteristicsW, GetCurrentThreadId};
use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

// ---- P9.5 control plane: main→audio event ring + main→owner state requests -----------------

/// One control event crossing the main→audio rtrb ring. `Copy` POD (the param id stays a raw
/// `u32`; the RT side validates it with `ClapId::from_raw` so an invalid id can never panic the
/// audio thread). Notes carry the MIDI key (0..127) and CLAP 0..1 velocity.
#[derive(Clone, Copy)]
pub enum PluginEvent {
    NoteOn { key: u16, velocity: f64 },
    NoteOff { key: u16 },
    Param { id: u32, value: f64 },
}

/// rtrb depth (events buffered between a command push and the next RT drain). Generous: at the
/// ~10 ms WASAPI block the audio thread drains up to `MAX_EVENTS_PER_BLOCK` each block (≫ any human
/// or single-knob input rate), so overflow is unreachable in practice — it's counted, not
/// coalesced (`events_dropped` in the gate). True per-paramId coalescing is deferred until a
/// param/automation UI can actually outrun this (post-P9.5); the deep ring makes it moot now.
const EVENT_RING_CAP: usize = 1024;
/// Hard cap on events drained per block. Bounds the pre-grown `EventBuffer` so its push path stays
/// alloc-free (never exceed the reserved capacity — invariant #5). 256 @ 10 ms = 25 600 ev/s.
const MAX_EVENTS_PER_BLOCK: usize = 256;
/// A request from a command thread to the owner (clack main) thread, which owns the `!Send`
/// `PluginInstance`. A parameter listing or a state save is a CLAP main-thread call, so it can't run on
/// the command thread — it's handed here with a one-shot reply channel the caller blocks on.
///
/// The two REVERSIBLE MUTATING requests (editor open/close) also carry an `Arc<AtomicBool>`
/// CANCELLATION token. Dropping the reply receiver on a timeout does NOT cancel a queued request,
/// and a hosted GUI can block this thread past the caller's 5 s wait — so the owner must know the
/// caller gave up: it skips the not-yet-started request (`take_uncancelled`) and ROLLS BACK a late
/// success (the `OpenEditor` arms close what they opened). `SaveTone`/`ListParams` carry no token:
/// none of them changes what the plugin plays. No request pushes state INTO a running plugin: a tone
/// is restored only inside a load (`host/tone.rs`).
pub enum OwnerRequest {
    /// Save the plugin's tone now (a session export takes it fresh), keep it in the store, and reply
    /// with the tone file's bytes (empty: the plugin keeps no state).
    SaveTone(std::sync::mpsc::SyncSender<Result<Vec<u8>, String>>),
    ListParams(std::sync::mpsc::SyncSender<Result<Vec<ParamDesc>, String>>),
    /// VST3 only: mirror a HOST-originated parameter set (`plugin_set_param`) to the edit
    /// controller with `IEditController::setParamNormalized`. The processor already gets the value
    /// through the main→audio ring; the controller is a separate object and the VST3 host contract
    /// says the host keeps it in sync — it is what the plugin's GUI displays and what decides a
    /// controller-side `restartComponent` (a FabFilter latency-mode change is decided there). Never
    /// sent for CLAP (one object, the param event is enough); no reply, never cancelled.
    SetParamNormalized(u32, f64),
    /// Sent by an unload right after `running=false`, so an owner blocked in `recv_timeout` wakes
    /// at once. Carries nothing; the loop re-checks `running` on its next turn.
    Wake,
    /// P10.0: open the floating editor (gui ext is a main-thread call). Reply Ok=opened.
    OpenEditor(
        Arc<AtomicBool>,
        std::sync::mpsc::SyncSender<Result<(), String>>,
    ),
    /// P10.0: hide + destroy the editor (main-thread). Reply Ok always (idempotent).
    CloseEditor(
        Arc<AtomicBool>,
        std::sync::mpsc::SyncSender<Result<(), String>>,
    ),
}

/// The parameter ids the host last enumerated for a slot (`listParams`: at load, on every
/// `plugin_list_params`, and when the plugin reports a rescan). Written only by the owner thread,
/// read by command threads; the audio thread never sees it. `plugin_set_param` checks against it
/// because an id the plugin never listed can crash the plugin (Surge's ids are hash-like).
pub(super) type ParamIds = Arc<std::sync::RwLock<std::collections::HashSet<u32>>>;

/// Owner-side: replace the known id set with what the plugin just listed.
pub(super) fn publish_param_ids(ids: &ParamIds, params: &[ParamDesc]) {
    if let Ok(mut set) = ids.write() {
        set.clear();
        set.extend(params.iter().map(|p| p.id));
    }
}

/// Command-side path for the REVERSIBLE mutating requests (editor open/close): mint the
/// cancellation token + reply channel, hand `build`'s request to the owner thread, block ≤5 s. On
/// expiry the token is stored BEFORE the error returns, because the caller books the operation as
/// FAILED the moment it sees that error — from then on the owner skips the mutation or rolls back a
/// late success. `op` names the call in the timeout message. Nothing times out ⇒ no behaviour change: the token stays false and every owner-side
/// check is one relaxed load.
fn owner_request_5s(
    req_tx: &std::sync::mpsc::Sender<OwnerRequest>,
    op: &str,
    build: impl FnOnce(
        Arc<AtomicBool>,
        std::sync::mpsc::SyncSender<Result<(), String>>,
    ) -> OwnerRequest,
) -> Result<(), String> {
    let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
    let cancelled = Arc::new(AtomicBool::new(false));
    req_tx
        .send(build(cancelled.clone(), reply_tx))
        .map_err(|_| "owner thread gone".to_string())?;
    match reply_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(res) => res,
        Err(e) => {
            cancelled.store(true, Relaxed);
            Err(format!("{op} timed out: {e}"))
        }
    }
}

/// Enumerate the plugin's parameters (host-side `params` ext, a main-thread call): stable ids,
/// ranges and the live value. Also what refreshes the slot's known-id set (`ParamIds`).
fn clap_param_descs(instance: &mut PluginInstance<LfHost>) -> Result<Vec<ParamDesc>, String> {
    let mut handle = instance.plugin_handle();
    let params = handle
        .get_extension::<PluginParams>()
        .ok_or_else(|| "plugin has no params extension".to_string())?;
    let count = params.count(&mut handle);
    let mut out = Vec::with_capacity(checked_plugin_params(count as i64, "CLAP params")?);
    let mut buf = ParamInfoBuffer::new();
    for i in 0..count {
        if let Some(info) = params.get_info(&mut handle, i, &mut buf) {
            let id = info.id;
            let (min_value, max_value, default_value) =
                (info.min_value, info.max_value, info.default_value);
            let name = String::from_utf8_lossy(info.name).into_owned();
            // The LIVE value, so a drawer opened after a preset load or a state restore
            // shows where the plugin actually is (falls back to default if unreadable).
            let value = params.get_value(&mut handle, id).unwrap_or(default_value);
            out.push(ParamDesc {
                id: id.get(),
                name,
                min_value,
                max_value,
                default_value,
                value,
            });
        }
    }
    Ok(out)
}

/// Owner-side: re-list the plugin's params into the known-id set (load, rescan). A plugin without
/// the params ext lists nothing, so every host-side set is refused.
fn refresh_clap_param_ids(instance: &mut PluginInstance<LfHost>, param_ids: &ParamIds) {
    publish_param_ids(param_ids, &clap_param_descs(instance).unwrap_or_default());
}

/// Owner-thread side: service one request on clack's main thread (holds the `!Send` instance).
fn handle_owner_request(
    req: OwnerRequest,
    instance: &mut PluginInstance<LfHost>,
    param_ids: &ParamIds,
) {
    match req {
        OwnerRequest::ListParams(reply) => {
            let res = clap_param_descs(instance);
            if let Ok(params) = &res {
                publish_param_ids(param_ids, params);
            }
            let _ = reply.send(res);
        }
        // The owner loop (`clap_engine`) serves these before it gets here (they need owner-local
        // state: the editor slot, the tone keeper). This arm only keeps the match exhaustive and
        // replies an error if one is ever misrouted here.
        OwnerRequest::SaveTone(reply) => {
            let _ = reply.send(Err("owner-local request misrouted to handle_owner_request".to_string()));
        }
        OwnerRequest::OpenEditor(_, reply) | OwnerRequest::CloseEditor(_, reply) => {
            let _ = reply.send(Err("owner-local request misrouted to handle_owner_request".to_string()));
        }
        // VST3-only mirror (`set_param` sends it only to a VST3 slot); a CLAP plugin has no
        // separate controller to keep in sync.
        OwnerRequest::SetParamNormalized(..) => {}
        OwnerRequest::Wake => {}
    }
}

/// Owner-side PRE-START cancellation gate, shared by both owner loops (CLAP + VST3). `None` = the
/// caller's 5 s wait expired before the owner reached this request, so it must not run at all: the
/// frontend has booked the operation as failed, and the reply drops with the request (an ignored
/// send is already the norm here). A request that PASSES here can still be cancelled mid-call —
/// that's the `OpenEditor` arms' rollback, not this gate.
///
/// Gated: `OpenEditor`, which would ADD a window late, and `CloseEditor`, whose caller keeps showing
/// the editor as OPEN when the close errors (`PluginControls.tsx`), so skipping is what both sides
/// agree on.
fn take_uncancelled(req: OwnerRequest, slot: u8) -> Option<OwnerRequest> {
    let cancelled_op = match &req {
        OwnerRequest::OpenEditor(c, _) => Some((c.load(Relaxed), "openEditor")),
        OwnerRequest::CloseEditor(c, _) => Some((c.load(Relaxed), "closeEditor")),
        _ => None,
    };
    if let Some((true, op)) = cancelled_op {
        log::warn!("[plugin_host] slot {slot} {op}: owner-request cancelled before start (caller timed out) — skipped, nothing changed");
        return None;
    }
    Some(req)
}

// ── P10.0 plugin editor: a plugin-owned FLOATING window first; else EMBEDDED into a host-owned
//    top-level window the owner thread pumps. The host window is OUR OWN top-level (owned by the
//    main window for z-order), NOT reparented into the WebView2 surface — so no airspace z-fight.

/// What's currently open for the slot's editor (owner-thread-local; never crosses threads).
enum EditorSlot {
    Closed,
    /// The plugin created + pumps its own floating window (no host window). Close arrives via the
    /// CLAP `gui.closed` callback (the `EditorClosed` flag).
    Floating,
    /// The plugin embedded its view into our host-owned window. Needs us to pump messages; close
    /// arrives via the window's `WM_CLOSE` (the `HostWindow` flag). Drop = `DestroyWindow`.
    Hosted(HostWindow),
}

/// P10.0 owner-thread: open the slot's plugin editor. Tries a plugin-owned FLOATING window first
/// (no host window / pump needed — the plugin self-manages it); if the plugin doesn't support a
/// WIN32 floating GUI (e.g. JUCE plugins like Surge XT), falls back to EMBEDDED into a host-owned
/// top-level window. `host_hwnd` (0 = unknown) is the main window, used as transient parent / owner.
fn editor_open(
    instance: &mut PluginInstance<LfHost>,
    host_hwnd: usize,
    hosted_hwnd: &AtomicIsize,
) -> Result<EditorSlot, String> {
    let mut handle = instance.plugin_handle();
    let gui = handle
        .get_extension::<PluginGui>()
        .ok_or_else(|| "plugin has no gui extension".to_string())?;

    // 1. Preferred path: a plugin-owned floating window.
    let floating = GuiConfiguration {
        api_type: GuiApiType::WIN32,
        is_floating: true,
    };
    if gui.is_api_supported(&mut handle, floating) {
        gui.create(&mut handle, floating)
            .map_err(|e| format!("gui.create(floating): {e}"))?;
        if host_hwnd != 0 {
            let parent = Window::from_win32_hwnd(host_hwnd as *mut core::ffi::c_void);
            // SAFETY: host_hwnd is the live main-window HWND; the editor is destroyed before the
            // app window. set_transient is best-effort (pin-above).
            unsafe {
                let _ = gui.set_transient(&mut handle, parent);
            }
        }
        if let Ok(title) = CString::new("BleepLoop Plugin") {
            gui.suggest_title(&mut handle, &title);
        }
        // Destroy the created GUI on failure — leaving it created makes the next open a double
        // gui.create (CLAP-contract violation). (Bug-hunt 2026-06-21, #3.)
        if let Err(e) = gui.show(&mut handle) {
            let _ = gui.hide(&mut handle);
            gui.destroy(&mut handle);
            return Err(format!("gui.show(floating): {e}"));
        }
        return Ok(EditorSlot::Floating);
    }

    // 2. Fallback: embed into a host-owned top-level window.
    let embedded = GuiConfiguration {
        api_type: GuiApiType::WIN32,
        is_floating: false,
    };
    if !gui.is_api_supported(&mut handle, embedded) {
        return Err("plugin supports neither WIN32 floating nor embedded GUI".to_string());
    }
    gui.create(&mut handle, embedded)
        .map_err(|e| format!("gui.create(embedded): {e}"))?;
    // After a successful gui.create, EVERY error path below must gui.destroy before returning —
    // else the created GUI leaks and the next editor_open does a double gui.create (CLAP-contract
    // violation, crashes JUCE). gui.destroy runs BEFORE host_win drops (DestroyWindow), matching the
    // teardown order (plugin removes its child first). (Bug-hunt 2026-06-21, #3.)
    let size = gui.get_size(&mut handle).unwrap_or(GuiSize {
        width: 900,
        height: 600,
    });
    let owner = if host_hwnd != 0 {
        Some(HWND(host_hwnd as *mut core::ffi::c_void))
    } else {
        None
    };
    let host_win = match create_host_window(size.width, size.height, owner) {
        Ok(w) => w,
        Err(e) => {
            let _ = gui.hide(&mut handle);
            gui.destroy(&mut handle);
            return Err(e);
        }
    };
    let parent = Window::from_win32_hwnd(host_win.hwnd.0 as *mut core::ffi::c_void);
    // SAFETY: host_win.hwnd was just created on this thread and outlives the plugin GUI (the
    // editor is `gui.destroy`d before HostWindow drops the window).
    let parented = unsafe { gui.set_parent(&mut handle, parent) };
    if let Err(e) = parented {
        let _ = gui.hide(&mut handle);
        gui.destroy(&mut handle);
        return Err(format!("gui.set_parent: {e}"));
    }
    if let Err(e) = gui.show(&mut handle) {
        let _ = gui.hide(&mut handle);
        gui.destroy(&mut handle);
        return Err(format!("gui.show(embedded): {e}"));
    }
    show_host_window_front(host_win.hwnd);
    // From here the plugin's `request_resize` has a window to resize; one left from an earlier
    // editor is forgotten first.
    instance.access_shared_handler(|s| s.pending_resize.store(NO_RESIZE, Release));
    hosted_hwnd.store(host_win.hwnd.0 as isize, Release);
    log::info!(
        "[plugin_host] editor embedded into host window ({}x{})",
        size.width,
        size.height
    );
    Ok(EditorSlot::Hosted(host_win))
}

/// Close whatever editor is open, in order: forget the hosted HWND (a late `request_resize` then
/// finds nothing), the plugin hides + destroys its GUI (removes its child first), our `HostWindow`
/// drops (DestroyWindow), and the teardown messages are pumped on this owner thread. Every close
/// path — user close box, floating `closed`, CloseEditor, the timed-out-open rollback, shutdown —
/// goes through here so none of them can forget a step.
fn editor_teardown(
    instance: &mut PluginInstance<LfHost>,
    editor: &mut EditorSlot,
    hosted_hwnd: &AtomicIsize,
) {
    hosted_hwnd.store(0, Release);
    editor_destroy_gui(instance);
    *editor = EditorSlot::Closed;
    drain_after_editor_teardown();
}

/// P10.0 owner-thread: tell the plugin to hide + free its GUI. For a hosted editor the caller then
/// drops the `HostWindow` (DestroyWindow) — do that AFTER, so the plugin removes its child first.
fn editor_destroy_gui(instance: &mut PluginInstance<LfHost>) {
    let mut handle = instance.plugin_handle();
    if let Some(gui) = handle.get_extension::<PluginGui>() {
        let _ = gui.hide(&mut handle);
        gui.destroy(&mut handle);
    }
}

// ---- CLAP host callbacks (verified vs clack-host 0.1.0 source) ----------------------------

/// P10.0: cross-thread signal from the plugin's GUI thread → the owner loop. CLAP dispatches
/// `gui.closed` to the host's `Shared` handler (NOT the main-thread handler), and a JUCE plugin
/// may call it from its own window thread — so `closed` only sets these flags; the owner loop
/// swaps `fired` each tick and acks the close (destroy + notify JS) on the main thread.
#[derive(Default)]
struct EditorClosed {
    fired: AtomicBool,
    was_destroyed: AtomicBool,
}

struct LfShared {
    editor_closed: Arc<EditorClosed>,
    /// The host window a HOSTED editor is embedded in (0 = none / floating). Set by `editor_open`,
    /// cleared by `editor_teardown`; `request_resize` resizes it. Shared with the owner loop.
    hosted_hwnd: Arc<AtomicIsize>,
    /// The latest size a plugin asked for off the window's thread (`width << 32 | height`,
    /// `NO_RESIZE` = none), which the owner applies on its turn (`deliver_pending_resize`).
    pending_resize: AtomicU64,
    callback_requested: AtomicBool,
    /// Set by `request_restart` (any thread), drained by the owner loop into ONE restart cycle.
    restart_requested: AtomicBool,
    /// The owner keeps the plugin's tone (`host/tone.rs`): the host declares the `state`
    /// extension, so the plugin can report a change with `mark_dirty`. The test fixtures build
    /// hosts without it.
    keeps_tone: bool,
}
/// `LfShared::pending_resize` when no request waits.
const NO_RESIZE: u64 = u64::MAX;

impl LfShared {
    /// Owner turn: resize the hosted window to the size a plugin asked for off its thread, if any.
    /// Returns the size the window kept when it could not take the request, which the plugin must be
    /// told (CLAP's revert of an acknowledged request).
    fn apply_pending_resize(&self) -> Option<(u32, u32)> {
        let packed = self.pending_resize.swap(NO_RESIZE, Acquire);
        let hwnd = self.hosted_hwnd.load(Acquire);
        if packed == NO_RESIZE || hwnd == 0 {
            return None;
        }
        let hwnd = HWND(hwnd as *mut core::ffi::c_void);
        let before = super::editor_window::client_size(hwnd);
        resize_hosted(hwnd, (packed >> 32) as u32, packed as u32).err().map(|_| before)
    }
}

/// Resize a hosted editor's window on the window's own thread. CLAP's `true` means "the client area
/// IS width×height now", so a size the screen clamps is refused and the window put back: the plugin
/// keeps laying out for the size it has.
fn resize_hosted(hwnd: HWND, width: u32, height: u32) -> Result<(), &'static str> {
    let before = super::editor_window::client_size(hwnd);
    match set_client_size(hwnd, width, height) {
        Some(got) if got == (width, height) => Ok(()),
        Some(_) => {
            let _ = set_client_size(hwnd, before.0, before.1);
            Err("requested editor size does not fit the screen")
        }
        None => Err("host window resize failed"),
    }
}

/// Owner turn, hosted editor open: apply a resize the plugin asked for off this thread, and tell the
/// plugin the size it has when the window could not take it.
fn deliver_pending_resize(instance: &mut PluginInstance<LfHost>) {
    let Some((width, height)) = instance.access_shared_handler(|s| s.apply_pending_resize()) else { return };
    let mut handle = instance.plugin_handle();
    if let Some(gui) = handle.get_extension::<PluginGui>() {
        let _ = gui.set_size(&mut handle, GuiSize { width, height });
    }
}

impl<'a> SharedHandler<'a> for LfShared {
    /// [thread-safe] The plugin needs a deactivate → activate cycle (its latency, ports or internal
    /// buffers changed). Flag only — never foreign code, allocation or a lock here; the owner loop
    /// runs the cycle on clack's main thread within one poll interval.
    fn request_restart(&self) {
        self.restart_requested.store(true, Release);
    }
    // The host continuously processes active plugins, so there is no sleeping processor to wake.
    fn request_process(&self) {}
    fn request_callback(&self) {
        // May run during init or on the audio thread. Never invoke foreign main-thread code here,
        // allocate a channel message, or take a lock. The owner's bounded poll delivers the request.
        self.callback_requested.store(true, Release);
    }
}

impl HostParamsImplShared for LfShared {
    /// The RT loop calls `process()` every block while the plugin is active, which is the flush CLAP
    /// asks for; the only inactive window is the restart cycle itself, after which process resumes.
    fn request_flush(&self) {}
}

/// Owner-thread host data (clack's main-thread handler). `params_rescan` is set by the plugin's
/// `clap_host_params.rescan` — a main-thread call meaning it changed parameter values or info behind
/// the host's back (a preset loaded in its own GUI) — and drained once per owner turn into a single
/// `plugin:params-changed` emit, so the web UI re-lists instead of showing stale sliders.
#[derive(Default)]
struct LfMain {
    params_rescan: bool,
    /// The plugin's `clap_host_state.mark_dirty`: its state changed (engine mode saves its tone).
    state_dirty: bool,
}
impl<'a> MainThreadHandler<'a> for LfMain {}
impl HostStateImpl for LfMain {
    fn mark_dirty(&mut self) {
        self.state_dirty = true;
    }
}
impl HostParamsImplMainThread for LfMain {
    fn rescan(&mut self, _flags: ParamRescanFlags) {
        self.params_rescan = true;
    }
    // Nothing to clear: the host keeps no automation or modulation references to a parameter.
    fn clear(&mut self, _param_id: ClapId, _flags: ParamClearFlags) {}
}

/// Owner turn: take the pending params-rescan request (true at most once per plugin request).
fn take_params_rescan(instance: &mut PluginInstance<LfHost>) -> bool {
    instance.access_handler_mut(|m| std::mem::take(&mut m.params_rescan))
}

/// Deliver at most one callback per owner turn. Clear BEFORE calling foreign code so a callback
/// requested reentrantly remains pending for the next turn instead of being lost or spinning here.
fn deliver_plugin_callback(instance: &mut PluginInstance<LfHost>) {
    let requested = instance.access_shared_handler(|shared| {
        shared.callback_requested.swap(false, Acquire)
    });
    if requested {
        instance.call_on_main_thread_callback();
    }
}

#[cfg(test)]
#[path = "clap_callback_fixture.rs"]
mod callback_tests;
#[cfg(test)]
#[path = "clap_restart_fixture.rs"]
mod restart_tests;

/// The hosted-editor `request_resize` contract, against a real host window and no plugin: refused
/// while no hosted editor is open (floating or closed), honoured — client area exactly the
/// requested size — once `editor_open` has published the window, refused again after teardown
/// forgets it.
#[cfg(test)]
mod resize_tests {
    use super::super::editor_window::{client_size, create_host_window};
    use super::*;

    fn shared(hosted_hwnd: &Arc<AtomicIsize>) -> LfShared {
        LfShared {
            editor_closed: Arc::new(EditorClosed::default()),
            hosted_hwnd: hosted_hwnd.clone(),
            pending_resize: AtomicU64::new(NO_RESIZE),
            callback_requested: AtomicBool::new(false),
            restart_requested: AtomicBool::new(false),
            keeps_tone: false,
        }
    }

    #[test]
    fn request_resize_follows_the_hosted_window_lifecycle() {
        let hosted_hwnd = Arc::new(AtomicIsize::new(0));
        let shared = shared(&hosted_hwnd);
        let size = |w, h| GuiSize {
            width: w,
            height: h,
        };
        assert!(shared.request_resize(size(640, 480)).is_err(), "no hosted editor → refused");

        let win = create_host_window(400, 300, None).expect("host window");
        hosted_hwnd.store(win.hwnd.0 as isize, Release);
        assert!(shared.request_resize(size(640, 480)).is_ok());
        assert_eq!(client_size(win.hwnd), (640, 480));
        assert!(shared.request_resize(size(320, 200)).is_ok());
        assert_eq!(client_size(win.hwnd), (320, 200));

        hosted_hwnd.store(0, Release); // what editor_teardown does first
        assert!(shared.request_resize(size(800, 600)).is_err(), "after teardown → refused");
        assert_eq!(client_size(win.hwnd), (320, 200), "window untouched by the refused call");
    }

    /// A request from a thread other than the window's (the plugin's audio or worker thread) returns
    /// while the owner is not pumping, and resizes nothing there.
    #[test]
    fn a_request_from_another_thread_never_waits_for_the_owner() {
        let hosted_hwnd = Arc::new(AtomicIsize::new(0));
        let shared = Arc::new(shared(&hosted_hwnd));
        let win = create_host_window(400, 300, None).expect("host window");
        hosted_hwnd.store(win.hwnd.0 as isize, Release);
        let ask = |width, height| {
            let shared = shared.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            let caller = std::thread::spawn(move || {
                let _ = tx.send(shared.request_resize(GuiSize { width, height }).is_ok());
            });
            // This thread does not pump: a request that waits for it never answers.
            let answered = rx.recv_timeout(Duration::from_millis(500));
            while !caller.is_finished() {
                pump_thread_messages(); // release a caller that did wait, so the test fails, not hangs
            }
            caller.join().unwrap();
            answered.expect("request_resize waited for the window's thread")
        };
        assert!(ask(640, 480), "acknowledged");
        assert!(ask(500, 400), "acknowledged");
        assert_eq!(client_size(win.hwnd), (400, 300), "nothing resized off the window's thread");
        assert_eq!(shared.apply_pending_resize(), None, "the owner's turn takes it");
        assert_eq!(client_size(win.hwnd), (500, 400), "the latest request wins");
        assert_eq!(shared.apply_pending_resize(), None, "nothing left");
        assert_eq!(client_size(win.hwnd), (500, 400));

        assert!(ask(100_000, 100_000), "acknowledged, though no screen fits it");
        assert_eq!(shared.apply_pending_resize(), Some((500, 400)), "the plugin is told the size it has");
        assert_eq!(client_size(win.hwnd), (500, 400), "and the window put back");

        // A request on the window's own thread after a queued one is the newer: the owner's turn must
        // not put the queued size over it.
        assert!(ask(640, 480));
        assert!(shared.request_resize(GuiSize { width: 360, height: 240 }).is_ok(), "the window's thread resizes at once");
        assert_eq!(shared.apply_pending_resize(), None);
        assert_eq!(client_size(win.hwnd), (360, 240), "the newer request stays");

        assert!(ask(320, 200));
        hosted_hwnd.store(0, Release); // the editor closed before the owner's turn
        assert_eq!(shared.apply_pending_resize(), None, "dropped");
        assert_eq!(client_size(win.hwnd), (360, 240));
    }
}
/// Host side of the CLAP `gui` extension (P10.0). For a floating editor the plugin manages its own
/// window, so the only callback that matters is `closed` (user-dismiss → re-sync) and the
/// resize/show/hide requests don't apply. For a HOSTED editor `request_resize` is real: the plugin
/// wants our window's client area at a new size (its size menu / zoom). On the window's thread a
/// `true` answer means the host resized and need not call `set_size` back; from another thread it
/// acknowledges a request the owner applies on its turn, or reverts with `set_size` (CLAP's gui.h
/// allows that asynchronous answer).
impl HostGuiImpl for LfShared {
    fn resize_hints_changed(&self) {}
    /// [thread-safe] Resize the hosted editor's window client area. On the window's thread (the
    /// owner, CLAP's main thread) it resizes at once. From any other thread (the audio thread
    /// included) it only records the size and acknowledges: a cross-thread `SetWindowPos` waits for
    /// the owner to pump, so the owner applies it on its turn (`deliver_pending_resize`), the latest
    /// request winning, and reverts the plugin with `set_size` if the window cannot take it.
    fn request_resize(&self, new_size: GuiSize) -> Result<(), HostError> {
        let hwnd = self.hosted_hwnd.load(Acquire);
        if hwnd == 0 {
            return Err(HostError::Message("resize not supported for a floating editor"));
        }
        let hwnd = HWND(hwnd as *mut core::ffi::c_void);
        // SAFETY: plain thread-id queries; a dead handle answers 0, which is no thread.
        if unsafe { GetWindowThreadProcessId(hwnd, None) != GetCurrentThreadId() } {
            let packed = (new_size.width as u64) << 32 | new_size.height as u64;
            self.pending_resize.store(packed, Release);
            return Ok(());
        }
        // Newer than any size another thread queued: that one must not land over this one later.
        self.pending_resize.store(NO_RESIZE, Release);
        resize_hosted(hwnd, new_size.width, new_size.height).map_err(HostError::Message)
    }
    fn request_show(&self) -> Result<(), HostError> {
        Ok(())
    }
    fn request_hide(&self) -> Result<(), HostError> {
        Ok(())
    }
    fn closed(&self, was_destroyed: bool) {
        self.editor_closed.was_destroyed.store(was_destroyed, Relaxed);
        self.editor_closed.fired.store(true, Release);
    }
}

struct LfHost;
impl HostHandlers for LfHost {
    type Shared<'a> = LfShared;
    type MainThread<'a> = LfMain;
    type AudioProcessor<'a> = ();

    /// P10.0: declare the host-side `gui` extension so the plugin can find our `clap_host_gui` and
    /// call `closed` when its floating window is dismissed. The default `declare_extensions` is
    /// empty — without this override `closed` never fires.
    fn declare_extensions(builder: &mut HostExtensions<Self>, shared: &Self::Shared<'_>) {
        // `params`: the plugin's rescan/clear/request_flush callbacks (without it a preset loaded in
        // the plugin's GUI leaves the web UI's sliders stale).
        builder.register::<HostGui>().register::<HostParams>();
        if shared.keeps_tone {
            builder.register::<HostState>();
        }
    }
}

// ---- MMCSS promotion (windows 0.61.3) -------------------------------------------------------

/// Promote the CALLING thread to the MMCSS "Pro Audio" class (the DEV spike's device callbacks).
fn promote_pro_audio() -> Option<HANDLE> {
    let mut task_index: u32 = 0;
    // SAFETY: FFI into avrt.dll; `w!` is a 'static NUL-terminated wide literal.
    unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index).ok() }
}

/// The owner → load command channel of an engine slot's load (`engine_slot::load`). Rendezvous
/// (capacity 0): a send that succeeds was received, so no result can sit unread in the channel when
/// the command's 15 s timeout drops the receiver; a late one goes back to the owner, which undoes it.
/// With a buffer, a result sent between that timeout and the drop would sit in it and leave with the
/// channel, its plugin never torn down.
fn load_ready_channel<T>() -> (std::sync::mpsc::SyncSender<T>, std::sync::mpsc::Receiver<T>) {
    std::sync::mpsc::sync_channel(0)
}

#[cfg(test)]
mod load_channel_tests {
    use super::*;

    /// The window the rendezvous closes: the load command's receiver still exists but no longer
    /// receives (between its 15 s timeout and its drop). The load channel must refuse a result
    /// then, so it goes back to the owner instead of leaving with the channel.
    #[test]
    fn the_load_channel_holds_no_result_its_command_is_not_receiving() {
        let (ready_tx, ready_rx) = load_ready_channel::<Result<&str, String>>();
        assert!(
            matches!(
                ready_tx.try_send(Ok("loaded plugin")),
                Err(std::sync::mpsc::TrySendError::Full(Ok("loaded plugin")))
            ),
            "a result the command is not receiving must be refused, not buffered"
        );
        assert!(ready_rx.try_recv().is_err(), "nothing sits unread in the channel");
    }
}

/// Hard ceilings for rows allocated from foreign plugin metadata. A malformed plugin can report an
/// arbitrary count, and sizing an allocation from it aborts the whole process (an allocation
/// failure does not unwind); validating the count first turns that into the normal error path.
/// Channels: zero is valid only for an absent input bus.
const MAX_PLUGIN_CHANNELS: i64 = 64;
/// Parameters: the largest listing in the 30-plugin restart survey is 2855 (Surge XT VST3, hidden
/// ones excluded); the ceiling leaves wide room above that and bounds one listing to a few MB.
const MAX_PLUGIN_PARAMS: i64 = 1 << 16;

/// A plugin-reported channel count, checked before it sizes a buffer (CLAP ports, VST3 buses).
fn checked_plugin_channels(count: i64, bus: &str, allow_zero: bool) -> Result<u32, String> {
    let minimum = if allow_zero { 0 } else { 1 };
    checked_plugin_count(count, bus, "channel", minimum, MAX_PLUGIN_CHANNELS).map(|c| c as u32)
}

/// A plugin-reported parameter count, checked before it sizes a listing (CLAP and VST3).
fn checked_plugin_params(count: i64, source: &str) -> Result<usize, String> {
    checked_plugin_count(count, source, "parameter", 0, MAX_PLUGIN_PARAMS).map(|c| c as usize)
}

fn checked_plugin_count(
    count: i64,
    source: &str,
    kind: &str,
    minimum: i64,
    maximum: i64,
) -> Result<i64, String> {
    if (minimum..=maximum).contains(&count) {
        Ok(count)
    } else {
        Err(format!(
            "{source} reports unsupported {kind} count {count} (supported {minimum}..={maximum})"
        ))
    }
}

/// Output channel count of port 0 (host-side `audio-ports` ext). Defaults to stereo if absent.
fn query_out_channels(instance: &mut PluginInstance<LfHost>) -> Result<u32, String> {
    let mut handle = instance.plugin_handle();
    let count = match handle.get_extension::<PluginAudioPorts>() {
        Some(ports) => {
            let mut buf = AudioPortInfoBuffer::new();
            match ports.get(&mut handle, 0, false, &mut buf) {
                Some(info) => info.channel_count,
                None => 2,
            }
        }
        None => 2,
    };
    checked_plugin_channels(count as i64, "CLAP output port 0", false)
}

/// P11.1: input channel count of port 0 (host-side `audio-ports` ext, `is_input=true`). Defaults
/// to **0** (NOT stereo like the output) when there is no input bus — a synth must report 0 so the
/// unit keeps `InputAudioBuffers::empty()`. Never `.max(1)`:
/// 0 must stay 0 (same "don't assume, query" discipline as the Surge hash-param crash).
fn query_in_channels(instance: &mut PluginInstance<LfHost>) -> Result<u32, String> {
    let mut handle = instance.plugin_handle();
    let count = match handle.get_extension::<PluginAudioPorts>() {
        Some(ports) => {
            let mut buf = AudioPortInfoBuffer::new();
            match ports.get(&mut handle, 0, true, &mut buf) {
                Some(info) => info.channel_count,
                None => 0,
            }
        }
        None => 0,
    };
    checked_plugin_channels(count as i64, "CLAP input port 0", true)
}

fn sum_to_mono(out_bufs: &[Vec<f32>], mono: &mut [f32], block: usize, chans: usize) {
    if chans <= 1 {
        mono[..block].copy_from_slice(&out_bufs[0][..block]);
        return;
    }
    let scale = 1.0 / chans as f32;
    for i in 0..block {
        let mut s = 0.0f32;
        for c in out_bufs.iter() {
            s += c[i];
        }
        mono[i] = s * scale;
    }
}

/// P10.1 — native VST3 host (second format): the COM host objects, module loader, activation,
/// editor and parameter listing its engine unit (`vst3_engine.rs`) runs on, sharing this file's
/// `PluginEvent` ring and request types. Lives in a child module so its `vst3::Steinberg` imports
/// don't collide with clap's clack imports (both define `NoteOnEvent`/`Event`). VST3 "COM" is
/// Steinberg's FUnknown ABI (no apartments) — `ComPtr<I>` is `Send` for VST3 interfaces (com-scrape
/// emits explicit `unsafe impl Send`), so the processor handle crosses owner→RT directly, no wrapper.

#[path = "vst3.rs"]
mod vst3_host;

// Engine mode (briefing: `src-tauri/AGENTS.md` § Plugin hosting): the same plugins as units inside the native
// engine's callback, each with its own owner thread (`engine_io/plugins.rs` routes to them).
#[allow(dead_code)]
#[path = "engine_slot.rs"]
pub(crate) mod engine_slot;
#[allow(dead_code)]
#[path = "clap_engine.rs"]
mod clap_engine;
#[cfg(debug_assertions)]
pub(crate) use vst3_host::engine_spike_run;
