//! The native CLAP host RT machinery (Windows-only — clack + WASAPI + MMCSS + WebView2 SharedBuffer).
//! The gotchas are in `src-tauri/AGENTS.md`. Also hosts the VST3 second format (`vst3_host`, `vst3.rs`) as a child module — the CLAP and
//! VST3 owners share this file's control plane (event ring, request channel, `SlotHandle`, gate) and
//! `transport.rs`'s downstream pipe; only the upstream render differs per format.

use super::editor_window::{
    create_host_window, drain_after_editor_teardown, pump_thread_messages, set_client_size,
    show_host_window_front, wait_for_input, HostWindow,
};
use super::native_io::NativeIo;
use super::state::{ParamDesc, PluginDescriptor, PluginHostState, PluginInfo, SlotState};
use super::transport::{
    checked_plugin_channels, checked_plugin_params, create_shared_ring, force_device_rate,
    report_new_rt_faults, Hop1Pipe, InPipe, LoadReady, OutMonitorPipe, ProducerDiag, RtFault,
    SharedBufferHandle, HOP1_CAPACITY_FRAMES, LOAD_GEN, TARGET_FILL_SECONDS,
};

use std::ffi::CString;
use std::sync::atomic::{
    AtomicBool, AtomicIsize, AtomicU32,
    Ordering::{Acquire, Relaxed, Release},
};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tauri::Emitter;

use clack_extensions::audio_ports::{AudioPortInfoBuffer, PluginAudioPorts};
use clack_extensions::params::{
    HostParams, HostParamsImplMainThread, HostParamsImplShared, ParamClearFlags, ParamInfoBuffer,
    ParamRescanFlags, PluginParams,
};
use clack_extensions::gui::{
    GuiApiType, GuiConfiguration, GuiSize, HostGui, HostGuiImpl, PluginGui, Window,
};
#[cfg(debug_assertions)]
use clack_extensions::state::PluginState;
use clack_host::entry::PluginEntry;
use clack_host::host::{HostError, HostExtensions};
use clack_host::events::event_types::{NoteOffEvent, NoteOnEvent, ParamValueEvent};
use clack_host::events::io::EventBuffer;
use clack_host::events::Match;
use clack_host::prelude::*;
use clack_host::utils::{ClapId, Cookie};

use rtrb::{Consumer, Producer, RingBuffer};

use windows::core::w;
use windows::Win32::Foundation::{HANDLE, HWND, RPC_E_CHANGED_MODE};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW,
};

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
/// ~10 ms WASAPI block the RT thread drains up to `MAX_EVENTS_PER_BLOCK` each block (≫ any human
/// or single-knob input rate), so overflow is unreachable in practice — it's counted, not
/// coalesced (`events_dropped` in the gate). True per-paramId coalescing is deferred until a
/// param/automation UI can actually outrun this (post-P9.5); the deep ring makes it moot now.
const EVENT_RING_CAP: usize = 1024;
/// Hard cap on events drained per block. Bounds the pre-grown `EventBuffer` so its push path stays
/// alloc-free (never exceed the reserved capacity — invariant #5). 256 @ 10 ms = 25 600 ev/s.
const MAX_EVENTS_PER_BLOCK: usize = 256;
/// P11.0: depth (in mono f32 samples) of the cpal→RT audio-input ring. The cpal callback (owner
/// thread, producer) and the RT producer loop (consumer) run at slightly different cadences; a few
/// blocks of slack absorbs the jitter. ~8 blocks @ ~480 frames ≈ 4k; round up. Drop-on-full
/// (counted as `input_overruns`, NOT `input_starves` — a full ring underruns nobody) rather than
/// block the cpal thread.
const IN_RING_CAP: usize = 8192;
/// ASIO tier: cap the RT producer block (plugin render + resampler block + pacing period). The WASAPI
/// engine period is ~480 frames (~10ms); ASIO's hardware buffer is ~256 (~5.8ms), so matching the RT
/// block to it cuts the plugin-block latency AND drops both ring floors (input consume / monitor fill).
/// Safe because the DriftController is block_dt-normalized (KI·err·block_dt) — a smaller block changes
/// loop frequency, not loop dynamics. The load-time cap keys on startup-fixed `asio_available()`;
/// an ASIO-capable build keeps this finer block even during WASAPI fallback, which the rings absorb.
/// Explicit buffer-size picks remain uncapped up to `MAX_SELECTABLE_BLOCK`.
const ASIO_MAX_BLOCK_FRAMES: u32 = 256;
/// P11.3: depth (mono f32 samples) of the RT→cpal-out MONITOR ring (branch-1). The RT producer
/// (QPC-paced) pushes; the cpal output callback (card-clocked) pops. Sized well above the monitor
/// setpoint so the DriftController band fits with headroom (~170ms @48k); the controller holds the
/// fill near the monitor target setpoint (MONITOR_TARGET_WASAPI/ASIO). Drop-on-full / zero-fill.
const OUT_RING_CAP: usize = 8192;
/// A request from a command thread to the owner (clack main) thread, which owns the `!Send`
/// `PluginInstance`. State save/load is a CLAP main-thread call, so it can't run on the command
/// thread — it's handed here with a one-shot reply channel the caller blocks on.
///
/// The six REVERSIBLE MUTATING requests (editor open/close, input + monitor arm/disarm) also carry
/// an `Arc<AtomicBool>` CANCELLATION token. Dropping the reply receiver on a timeout does NOT
/// cancel a queued request, and a hosted GUI or a device open can block this thread past the
/// caller's 5 s wait — so the owner must know the caller gave up: it skips the not-yet-started
/// request (`take_uncancelled`, which lists the one exception) and ROLLS BACK a late success (the
/// `OpenEditor`/`Arm*` arms). Otherwise a late-succeeding monitor arm leaves a native stream running
/// that the frontend booked as failed — the wet then sounds twice (native monitor + the web path JS
/// never muted). `SaveState`/`LoadState`/`ListParams` carry no token (see `load_state`).
pub enum OwnerRequest {
    #[cfg(debug_assertions)]
    SaveState(std::sync::mpsc::SyncSender<Result<Vec<u8>, String>>),
    #[cfg(debug_assertions)]
    LoadState(Vec<u8>, std::sync::mpsc::SyncSender<Result<(), String>>),
    ListParams(std::sync::mpsc::SyncSender<Result<Vec<ParamDesc>, String>>),
    /// VST3 only: mirror a HOST-originated parameter set (`plugin_set_param`) to the edit
    /// controller with `IEditController::setParamNormalized`. The processor already gets the value
    /// through the main→audio ring; the controller is a separate object and the VST3 host contract
    /// says the host keeps it in sync — it is what the plugin's GUI displays and what decides a
    /// controller-side `restartComponent` (a FabFilter latency-mode change is decided there). Never
    /// sent for CLAP (one object, the param event is enough); no reply, never cancelled.
    SetParamNormalized(u32, f64),
    /// Sent by `SlotHandle::teardown` right after `running=false`: a VST3 owner with no editor
    /// open blocks in `recv_timeout` for up to its 2 s gate period, and a store to `running` does
    /// not wake it. Carries nothing; the loop re-checks `running` on its next turn.
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
    /// P11.0: arm a cpal capture stream (device_id; None = default) on input `channel` (None =
    /// auto) into the plugin input bus. Owner-thread-handled because the `!Send` cpal `Stream`
    /// lives there. Reply Ok=armed; Err if the plugin has no audio input bus (a synth) or the
    /// stream fails to open.
    ArmInput(
        Option<String>,
        Option<u32>,
        Arc<AtomicBool>,
        std::sync::mpsc::SyncSender<Result<(), String>>,
    ),
    /// P11.0: drop the cpal capture stream (idempotent). Reply Ok always. The token is carried for
    /// the uniform `owner_request_5s` path but DELIBERATELY never gated on: JS reconciles its state
    /// before the call, so skipping a "cancelled" disarm would leave a live native stream JS
    /// believes is gone — the exact divergence cancellation exists to prevent (`take_uncancelled`).
    DisarmInput(
        #[allow(dead_code)] Arc<AtomicBool>,
        std::sync::mpsc::SyncSender<Result<(), String>>,
    ),
    /// P11.3: arm the native low-latency MONITOR — open a cpal OUTPUT stream on `device_id`
    /// (None = default output) fed the plugin's wet mono (branch-1). Owner-thread-handled (the
    /// `!Send` cpal `Stream` lives there). Reply Ok=armed; Err if the stream fails to open.
    ArmMonitor(
        Option<String>,
        Arc<AtomicBool>,
        std::sync::mpsc::SyncSender<Result<(), String>>,
    ),
    /// P11.3: drop the native monitor output stream (idempotent). Reply Ok always. Token carried
    /// but never gated on — same reasoning as `DisarmInput`.
    DisarmMonitor(
        #[allow(dead_code)] Arc<AtomicBool>,
        std::sync::mpsc::SyncSender<Result<(), String>>,
    ),
}

/// The parameter ids the host last enumerated for a slot (`listParams`: at load, on every
/// `plugin_list_params`, and when the plugin reports a rescan). Written only by the owner thread,
/// read by command threads; the RT thread never sees it. `plugin_set_param` checks against it
/// because an id the plugin never listed can crash the plugin (Surge's ids are hash-like).
pub(super) type ParamIds = Arc<std::sync::RwLock<std::collections::HashSet<u32>>>;

/// Owner-side: replace the known id set with what the plugin just listed.
pub(super) fn publish_param_ids(ids: &ParamIds, params: &[ParamDesc]) {
    if let Ok(mut set) = ids.write() {
        set.clear();
        set.extend(params.iter().map(|p| p.id));
    }
}

/// Push one event onto a slot's ring from a command thread (the producer Mutex is uncontended in
/// practice: the RT side only holds the Consumer). A full ring bumps `events_dropped` and returns an
/// error naming what was lost, so the caller (and the frontend) knows the plugin never got it: a
/// dropped note-off is a stuck note.
fn try_enqueue(
    tx: &std::sync::Mutex<Producer<PluginEvent>>,
    diag: &ProducerDiag,
    slot: u8,
    ev: PluginEvent,
) -> Result<(), String> {
    let mut prod = tx
        .lock()
        .map_err(|_| format!("slot {slot} event queue is unavailable (lock poisoned)"))?;
    if prod.push(ev).is_err() {
        diag.events_dropped.fetch_add(1, Relaxed);
        let what = match ev {
            PluginEvent::NoteOn { key, .. } => format!("note-on {key}"),
            PluginEvent::NoteOff { key } => format!("note-off {key}"),
            PluginEvent::Param { id, .. } => format!("parameter {id} change"),
        };
        return Err(format!("slot {slot} event queue is full; {what} was dropped"));
    }
    Ok(())
}

/// Command-side: push one `PluginEvent` onto a slot's ring. Clones the `Arc` handles out from
/// under the slots lock so the (microsecond) push doesn't hold it. A push to an empty slot is a
/// benign no-op (a note racing an unload); a full ring bumps `events_dropped` and returns `Err`.
pub fn enqueue_event(state: &PluginHostState, slot: u8, ev: PluginEvent) -> Result<(), String> {
    let handles = {
        let slots = state.slots.lock().map_err(|_| "slots lock poisoned".to_string())?;
        slots[slot as usize]
            .loaded()
            .map(|h| (h.event_tx.clone(), h.diag.clone()))
    };
    match handles {
        Some((tx, diag)) => try_enqueue(&tx, &diag, slot, ev),
        None => Ok(()),
    }
}

/// Command-side `plugin_set_param`: the id must be one the plugin listed (`ParamIds`), else `Err`
/// before anything reaches the ring. The value goes to the processor through the main→audio ring
/// (next block) and, on a VST3 slot, to the edit controller through the owner thread
/// (`OwnerRequest::SetParamNormalized`) — only when the ring push succeeded, so the plugin's GUI
/// never shows a value its processor did not get. An empty slot is a no-op (a set racing an
/// unload); a gone owner thread means the slot is unloading.
pub fn set_param(state: &PluginHostState, slot: u8, id: u32, value: f64) -> Result<(), String> {
    let handles = {
        let slots = state.slots.lock().map_err(|_| "slots lock poisoned".to_string())?;
        slots[slot as usize].loaded().map(|h| {
            (
                h.event_tx.clone(),
                h.diag.clone(),
                h.param_ids.clone(),
                (h.info.descriptor.format == "vst3").then(|| h.request_tx.clone()),
            )
        })
    };
    let Some((tx, diag, param_ids, mirror)) = handles else {
        return Ok(());
    };
    let known = param_ids.read().map(|set| set.contains(&id)).unwrap_or(false);
    if !known {
        return Err(format!(
            "parameter {id} is not one the plugin in slot {slot} listed"
        ));
    }
    try_enqueue(&tx, &diag, slot, PluginEvent::Param { id, value })?;
    if let Some(req_tx) = mirror {
        let _ = req_tx.send(OwnerRequest::SetParamNormalized(id, value));
    }
    Ok(())
}

/// Command-side: ask the owner thread to serialise the plugin and block (≤5 s) for the bytes.
#[cfg(debug_assertions)]
pub fn save_state(state: &PluginHostState, slot: u8) -> Result<Vec<u8>, String> {
    let req_tx = slot_request_tx(state, slot)?;
    let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
    req_tx
        .send(OwnerRequest::SaveState(reply_tx))
        .map_err(|_| "owner thread gone".to_string())?;
    reply_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|e| format!("save_state timed out: {e}"))?
}

/// Command-side: hand the owner thread bytes to restore and block (≤5 s) for the result.
/// RESIDUAL (known): unlike the six reversible requests this one carries no cancellation token, so
/// a `state.load` that finishes after the 5 s wait still applies to the plugin while the caller has
/// reported failure. Left as-is deliberately: restoring state has no inverse to roll back to (the
/// pre-load state is not captured anywhere), so cancelling it needs a design, not a token.
#[cfg(debug_assertions)]
pub fn load_state(state: &PluginHostState, slot: u8, bytes: Vec<u8>) -> Result<(), String> {
    let req_tx = slot_request_tx(state, slot)?;
    let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
    req_tx
        .send(OwnerRequest::LoadState(bytes, reply_tx))
        .map_err(|_| "owner thread gone".to_string())?;
    reply_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|e| format!("load_state timed out: {e}"))?
}

/// Command-side: ask the owner thread to enumerate the plugin's parameters (host-side `params`
/// ext is a main-thread call). Returns stable `id`s + ranges so a caller can set a real param.
pub fn list_params(state: &PluginHostState, slot: u8) -> Result<Vec<ParamDesc>, String> {
    let req_tx = slot_request_tx(state, slot)?;
    let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
    req_tx
        .send(OwnerRequest::ListParams(reply_tx))
        .map_err(|_| "owner thread gone".to_string())?;
    reply_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|e| format!("list_params timed out: {e}"))?
}

fn slot_request_tx(
    state: &PluginHostState,
    slot: u8,
) -> Result<std::sync::mpsc::Sender<OwnerRequest>, String> {
    let slots = state.slots.lock().map_err(|_| "slots lock poisoned")?;
    slots[slot as usize]
        .loaded()
        .map(|h| h.request_tx.clone())
        .ok_or_else(|| format!("no plugin loaded in slot {slot}"))
}

/// Command-side path for the six REVERSIBLE mutating requests: mint the cancellation token + reply
/// channel, hand `build`'s request to the owner thread, block ≤5 s. On expiry the token is stored
/// BEFORE the error returns, because the caller books the operation as FAILED the moment it sees
/// that error — from then on the owner skips the mutation or rolls back a late success (the two
/// disarms are the documented exception in `take_uncancelled`). `op` names the call in the timeout
/// message. Nothing times out ⇒ no behaviour change: the token stays false and every owner-side
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

/// P10.0 command-side: ask the owner thread to open the slot's floating editor; block (≤5 s).
/// A timeout cancels the request (a GUI that appears late is destroyed again).
pub fn open_editor(state: &PluginHostState, slot: u8) -> Result<(), String> {
    let req_tx = slot_request_tx(state, slot)?;
    owner_request_5s(&req_tx, "open_editor", OwnerRequest::OpenEditor)
}

/// P10.0 command-side: ask the owner thread to close (hide + destroy) the slot's editor; block.
pub fn close_editor(state: &PluginHostState, slot: u8) -> Result<(), String> {
    let req_tx = slot_request_tx(state, slot)?;
    owner_request_5s(&req_tx, "close_editor", OwnerRequest::CloseEditor)
}

/// P11.0 command-side: ask the owner thread to arm a cpal capture stream on `device_id`, channel
/// `channel` (None = auto), into the slot's plugin input bus; block (≤5 s). Errs if the plugin
/// has no input bus or the stream fails. A timeout cancels the request (a stream armed late is
/// disarmed again).
pub fn arm_input(
    state: &PluginHostState,
    slot: u8,
    device_id: Option<String>,
    channel: Option<u32>,
) -> Result<(), String> {
    let req_tx = slot_request_tx(state, slot)?;
    owner_request_5s(&req_tx, "arm_input", |cancelled, reply_tx| {
        OwnerRequest::ArmInput(device_id, channel, cancelled, reply_tx)
    })
}

/// P11.0 command-side: ask the owner thread to drop the slot's cpal capture stream; block.
/// Idempotent.
pub fn disarm_input(state: &PluginHostState, slot: u8) -> Result<(), String> {
    let req_tx = slot_request_tx(state, slot)?;
    owner_request_5s(&req_tx, "disarm_input", OwnerRequest::DisarmInput)
}

/// P11.3 command-side: ask the owner thread to arm the native monitor (open a cpal OUTPUT stream
/// on `device_id`, None = default) fed the slot's wet plugin output; block (≤5s). Errs if the
/// stream fails. Independent of input arming (a synth or an FX can be monitored). A timeout
/// cancels the request — else a late arm would be heard on top of the web monitor path JS kept up.
pub fn arm_monitor(
    state: &PluginHostState,
    slot: u8,
    device_id: Option<String>,
) -> Result<(), String> {
    let req_tx = slot_request_tx(state, slot)?;
    owner_request_5s(&req_tx, "arm_monitor", |cancelled, reply_tx| {
        OwnerRequest::ArmMonitor(device_id, cancelled, reply_tx)
    })
}

/// P11.3 command-side: ask the owner thread to drop the slot's native monitor stream; block.
/// Idempotent.
pub fn disarm_monitor(state: &PluginHostState, slot: u8) -> Result<(), String> {
    let req_tx = slot_request_tx(state, slot)?;
    owner_request_5s(&req_tx, "disarm_monitor", OwnerRequest::DisarmMonitor)
}

/// P11.3 command-side: set the slot's native-monitor output gain (linear, clamped ≥0). Stores
/// DIRECTLY into the slot's `monitor_gain` atomic — no owner-request hop, so a UI slider drag is
/// click-free and the RT cpal-out callback picks it up on its next callback. No-op-errs if the slot
/// holds no plugin. Wired on BOTH CLAP and VST3 slots (Stage B): this is the same Arc each format's
/// cpal-out callback reads every callback.
pub fn set_monitor_gain(state: &PluginHostState, slot: u8, gain: f32) -> Result<(), String> {
    let slots = state.slots.lock().map_err(|_| "slots lock poisoned")?;
    let h = slots[slot as usize]
        .loaded()
        .ok_or_else(|| format!("slot {slot} has no plugin loaded"))?;
    h.monitor_gain.store(gain.max(0.0).to_bits(), Relaxed);
    Ok(())
}

/// P11.3 record-latency: the slot's native-monitor output latency ("cpal_out") in SECONDS — the time
/// from the RT producer emitting a wet sample to the player hearing it through the cpal OUTPUT stream.
/// `= monitor_fill / monitor_rate + reported output latency`. The output term is the callback-local
/// median of playback minus callback timestamps; callback period is only the startup/unsupported
/// fallback. Returns 0.0 when the monitor is disarmed (`monitor_rate == 0`) or the slot holds no
/// plugin — i.e. no native monitor ⇒ no record-latency compensation. The looper's compensation SUBTRACTS
/// this term: the player aligns their natively-monitored guitar to the heard click, self-correcting for
/// it, so it cancels out of the recorded-take offset. Read-only (no owner-thread hop). The diag fields
/// it reads are RT-written (`monitor_fill`, `monitor_rate`) or owner-mirrored each gate emit
/// (device-latency median and fallback block), so a value is available shortly after arm.
pub fn monitor_latency_seconds(state: &PluginHostState, slot: u8) -> Result<f64, String> {
    let slots = state.slots.lock().map_err(|_| "slots lock poisoned")?;
    let h = match slots[slot as usize].loaded() {
        Some(h) => h,
        None => return Ok(0.0), // no plugin in this slot ⇒ no monitor ⇒ no compensation
    };
    let rate = h.diag.monitor_rate.load(Relaxed);
    if rate == 0 {
        return Ok(0.0); // monitor disarmed
    }
    let fill = h.diag.monitor_fill.load(Relaxed);
    let out_block = h.diag.monitor_out_block.load(Relaxed);
    let output_ns = h.diag.monitor_output_latency_ns.load(Relaxed);
    let device_seconds = if output_ns > 0 {
        output_ns as f64 / 1e9
    } else {
        out_block as f64 / rate as f64
    };
    Ok(fill as f64 / rate as f64 + device_seconds)
}

/// P11.3 command-side: publish one process-global RT buffer size + generation. Every producer loop
/// watches the generation and rebuilds at the new block without a plugin reload. A process-global
/// generation also covers a setting change while a slot is still loading and not yet parked.
pub fn set_buffer_size(frames: u32) {
    CHOSEN_BLOCK_FRAMES.store(frames, Relaxed);
    BLOCK_CONFIG_GEN.fetch_add(1, Release);
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
        #[cfg(debug_assertions)]
        OwnerRequest::SaveState(reply) => {
            let res = (|| -> Result<Vec<u8>, String> {
                let mut handle = instance.plugin_handle();
                let ext = handle
                    .get_extension::<PluginState>()
                    .ok_or_else(|| "plugin has no state extension".to_string())?;
                let mut buf: Vec<u8> = Vec::new();
                ext.save(&mut handle, &mut buf)
                    .map_err(|e| format!("state.save: {e}"))?;
                Ok(buf)
            })();
            let _ = reply.send(res);
        }
        #[cfg(debug_assertions)]
        OwnerRequest::LoadState(bytes, reply) => {
            let res = (|| -> Result<(), String> {
                let mut handle = instance.plugin_handle();
                let ext = handle
                    .get_extension::<PluginState>()
                    .ok_or_else(|| "plugin has no state extension".to_string())?;
                let mut cur = std::io::Cursor::new(&bytes[..]);
                ext.load(&mut handle, &mut cur)
                    .map_err(|e| format!("state.load: {e}"))?;
                Ok(())
            })();
            let _ = reply.send(res);
        }
        OwnerRequest::ListParams(reply) => {
            let res = clap_param_descs(instance);
            if let Ok(params) = &res {
                publish_param_ids(param_ids, params);
            }
            let _ = reply.send(res);
        }
        // P10.0 editor + P11.0 input requests are intercepted in the owner loop (they need
        // owner-local state: the host HWND / editor slot, or the `!Send` cpal Stream + input
        // producer). This arm only keeps the match exhaustive and replies an error if one is ever
        // misrouted here.
        OwnerRequest::OpenEditor(_, reply)
        | OwnerRequest::CloseEditor(_, reply)
        | OwnerRequest::ArmInput(_, _, _, reply)
        | OwnerRequest::DisarmInput(_, reply)
        | OwnerRequest::ArmMonitor(_, _, reply)
        | OwnerRequest::DisarmMonitor(_, reply) => {
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
/// that's the Arm*/OpenEditor arms' rollback, not this gate.
///
/// Gated: the requests that would ADD native state late — `OpenEditor`, `ArmInput`, `ArmMonitor` —
/// plus `CloseEditor`, whose caller keeps showing the editor as OPEN when the close errors
/// (`PluginControls.tsx`), so skipping is what both sides agree on.
/// NOT gated: `DisarmInput`/`DisarmMonitor`. Their caller reconciles to "gone" SYNCHRONOUSLY before
/// awaiting (`instrument.ts` `disarmMonitorInternal` unmutes the web path and clears the armed flag,
/// then swallows the error), so a skipped disarm would leave a live native monitor JS believes is
/// gone — the double-wet failure this cancellation exists to prevent. Late is exactly right there.
/// They still carry a token so all six reversible requests share one command path.
fn take_uncancelled(req: OwnerRequest, slot: u8) -> Option<OwnerRequest> {
    let cancelled_op = match &req {
        OwnerRequest::OpenEditor(c, _) => Some((c.load(Relaxed), "openEditor")),
        OwnerRequest::CloseEditor(c, _) => Some((c.load(Relaxed), "closeEditor")),
        OwnerRequest::ArmInput(_, _, c, _) => Some((c.load(Relaxed), "armInput")),
        OwnerRequest::ArmMonitor(_, c, _) => Some((c.load(Relaxed), "armMonitor")),
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
    // From here the plugin's `request_resize` has a window to resize.
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

/// Translate one `PluginEvent` into its CLAP event and push it onto the per-block `EventBuffer`.
/// All events are stamped time 0 (apply at block start) → trivially time-sorted. Push is
/// alloc-free while the buffer stays under its reserved capacity (drain is capped to guarantee it).
fn push_event(buf: &mut EventBuffer, ev: PluginEvent) {
    match ev {
        PluginEvent::NoteOn { key, velocity } => {
            buf.push(&NoteOnEvent::new(
                0,
                Pckn::new(0u16, 0u16, key, Match::All),
                velocity,
            ));
        }
        PluginEvent::NoteOff { key } => {
            buf.push(&NoteOffEvent::new(
                0,
                Pckn::new(0u16, 0u16, key, Match::All),
                0.0,
            ));
        }
        PluginEvent::Param { id, value } => {
            if let Some(cid) = ClapId::from_raw(id) {
                buf.push(&ParamValueEvent::new(
                    0,
                    cid,
                    Pckn::match_all(),
                    value,
                    Cookie::empty(),
                ));
            }
        }
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
    callback_requested: AtomicBool,
    /// Set by `request_restart` (any thread), drained by the owner loop into ONE `service_restart`.
    restart_requested: AtomicBool,
}
impl<'a> SharedHandler<'a> for LfShared {
    /// [thread-safe] The plugin needs a deactivate → activate cycle (its latency, ports or internal
    /// buffers changed). Flag only — never foreign code, allocation or a lock here; the owner loop
    /// runs the cycle (`service_restart`) on clack's main thread within one poll interval.
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
}
impl<'a> MainThreadHandler<'a> for LfMain {}
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

const CALLBACK_POLL_INTERVAL: Duration = Duration::from_millis(20);

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
            callback_requested: AtomicBool::new(false),
            restart_requested: AtomicBool::new(false),
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
}
/// Host side of the CLAP `gui` extension (P10.0). For a floating editor the plugin manages its own
/// window, so the only callback that matters is `closed` (user-dismiss → re-sync) and the
/// resize/show/hide requests don't apply. For a HOSTED editor `request_resize` is real: the plugin
/// wants our window's client area at a new size (its size menu / zoom) and, per the CLAP contract,
/// a `true` answer means the host resized and need not call `set_size` back.
impl HostGuiImpl for LfShared {
    fn resize_hints_changed(&self) {}
    /// [thread-safe] Resize the hosted editor's window client area. `set_client_size` is a plain
    /// `SetWindowPos` (Win32 marshals it to the window's thread), so no lock, allocation or
    /// plugin call happens here whichever thread the plugin used.
    fn request_resize(&self, new_size: GuiSize) -> Result<(), HostError> {
        let hwnd = self.hosted_hwnd.load(Acquire);
        if hwnd == 0 {
            return Err(HostError::Message("resize not supported for a floating editor"));
        }
        // CLAP's `true` means "the client area IS width×height now", so a size the screen clamps
        // is refused and the window put back — the plugin keeps laying out for the size it has.
        let hwnd = HWND(hwnd as *mut core::ffi::c_void);
        let before = super::editor_window::client_size(hwnd);
        match set_client_size(hwnd, new_size.width, new_size.height) {
            Some(got) if got == (new_size.width, new_size.height) => Ok(()),
            Some(_) => {
                let _ = set_client_size(hwnd, before.0, before.1);
                Err(HostError::Message("requested editor size does not fit the screen"))
            }
            None => Err(HostError::Message("host window resize failed")),
        }
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
    fn declare_extensions(builder: &mut HostExtensions<Self>, _shared: &Self::Shared<'_>) {
        // `params`: the plugin's rescan/clear/request_flush callbacks (without it a preset loaded in
        // the plugin's GUI leaves the web UI's sliders stale).
        builder.register::<HostGui>().register::<HostParams>();
    }
}

// ---- WASAPI device-period query + MMCSS promotion (windows 0.61.3, cpal-cross-checked) -----

/// One-shot query of the default render endpoint's WASAPI shared-mode device period, returned
/// as a frame count at `sample_rate`. Opens NO stream — cpal/WASAPI must never drive a second
/// clock (invariant #1); we only read the period to size the RT block + ring.
fn wasapi_period_frames(sample_rate: f64) -> Result<u32, String> {
    // SAFETY: FFI into the Windows audio stack; every pointer is valid for its call and every
    // returned COM object is owned (dropped via windows-rs RAII at scope end).
    unsafe {
        // MTA suits a non-UI producer; S_FALSE (already inited) and RPC_E_CHANGED_MODE
        // (apartment mismatch) are NOT errors — only balance CoUninitialize if we inited.
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        let com_owned = hr.is_ok();
        if hr.is_err() && hr != RPC_E_CHANGED_MODE {
            return Err(format!("CoInitializeEx failed: {hr:?}"));
        }

        let result = (|| -> Result<u32, String> {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .map_err(|e| format!("CoCreateInstance(MMDeviceEnumerator): {e}"))?;
            let device: IMMDevice = enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .map_err(|e| format!("GetDefaultAudioEndpoint: {e}"))?;
            let client: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| format!("IMMDevice::Activate(IAudioClient): {e}"))?;
            let mut default_period_100ns: i64 = 0;
            client
                .GetDevicePeriod(Some(&mut default_period_100ns), None)
                .map_err(|e| format!("GetDevicePeriod: {e}"))?;
            if default_period_100ns <= 0 {
                return Err("GetDevicePeriod returned non-positive period".into());
            }
            let frames = (default_period_100ns as f64 * 1e-7 * sample_rate).round();
            Ok(frames as u32)
        })();

        if com_owned {
            CoUninitialize();
        }
        result
    }
}

/// Promote the CALLING thread to the MMCSS "Pro Audio" class. Keep the handle for the thread's
/// lifetime; `revert` it before the thread exits.
fn promote_pro_audio() -> Option<HANDLE> {
    let mut task_index: u32 = 0;
    // SAFETY: FFI into avrt.dll; `w!` is a 'static NUL-terminated wide literal.
    unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index).ok() }
}

fn revert(h: HANDLE) {
    // SAFETY: `h` came from AvSetMmThreadCharacteristicsW on this thread.
    unsafe {
        let _ = AvRevertMmThreadCharacteristics(h);
    }
}

/// P11.3 live buffer-size control. The user-chosen RT block (frames), set by
/// `plugin_set_buffer_size`. 0 = "use the device/ASIO default" (whose resolved value may be a
/// NON-option frame count — e.g. the WASAPI ~480 period — clamped to `MAX_SELECTABLE_BLOCK`; only an
/// explicit pick is one of the JS `BUFFER_FRAMES_OPTIONS`). Read at plugin load (resolves the initial
/// block) and after every process-global config-generation change, so a live change re-paces without
/// a plugin reload.
static CHOSEN_BLOCK_FRAMES: AtomicU32 = AtomicU32::new(0);
/// Generation for `CHOSEN_BLOCK_FRAMES`. Store chosen first, then Release-bump; readers Acquire-load
/// this before reading chosen so a change cannot disappear while a plugin is still loading.
static BLOCK_CONFIG_GEN: AtomicU32 = AtomicU32::new(0);
/// Largest selectable RT block. Plugins activate with `MAX_SELECTABLE_BLOCK + 128` as
/// `max_frames_count`, so the live block can sweep up to here WITHOUT re-activating (max_frames is
/// an upper bound — the plugin may process fewer frames per block). Keep the ceiling in sync with
/// the JS `BUFFER_FRAMES_OPTIONS` (audio-settings.ts).
const MAX_SELECTABLE_BLOCK: u32 = 1024;

/// Validate the isolated DEV measurement target and snapshot transport loss counters.
#[cfg(debug_assertions)]
pub(crate) fn marker_probe_target(
    state: &PluginHostState,
    slot: u8,
) -> Result<(usize, [u64; 2]), String> {
    if slot > 1 {
        return Err("Invalid marker slot".into());
    }
    let slots = state.slots.lock().map_err(|_| "slots lock poisoned")?;
    if !matches!(slots[1 - slot as usize], SlotState::Empty) {
        return Err("Marker probe requires exactly one loaded slot".into());
    }
    let h = slots[slot as usize]
        .loaded()
        .ok_or("Marker probe needs a loaded plugin")?;
    if h.diag.monitor_rate.load(Relaxed) == 0 {
        return Err("Marker probe requires an armed native monitor".into());
    }
    Ok((
        Arc::as_ptr(&h.diag) as usize,
        [
            h.diag.frames_dropped.load(Relaxed),
            h.diag.monitor_overruns.load(Relaxed),
        ],
    ))
}

/// Per-slot control handle parked in `PluginHostState` (all `Send + Sync`). `info`/`diag` are
/// kept for P9.3+ (status surface / re-query). `shared_buf` keeps the WebView2 SharedBuffer's
/// COM object alive for the plugin's life; `Close()`d on the UI thread at unload
/// (`SharedBufferHandle`, `transport.rs`).
pub struct SlotHandle {
    info: PluginInfo,
    running: Arc<AtomicBool>,
    diag: Arc<ProducerDiag>,
    owner_join: std::thread::JoinHandle<()>,
    shared_buf: SharedBufferHandle,
    /// P9.5 main→audio event ring producer (Mutex so concurrent command-handler threads can
    /// push safely; the RT thread holds the Consumer and pops lock-free, never touching this).
    event_tx: Arc<std::sync::Mutex<Producer<PluginEvent>>>,
    /// P9.5 main→owner request channel (state save/load on clack's main thread). mpsc::Sender is
    /// multi-producer, so no Mutex needed.
    request_tx: std::sync::mpsc::Sender<OwnerRequest>,
    /// P11.3 native-monitor output gain (linear, default unity). The SAME `Arc` the owner thread
    /// hands the cpal OUTPUT callback (CLAP path) — so `set_monitor_gain` stores here DIRECTLY from
    /// the command thread (no owner-request hop ⇒ slider responsiveness), like a diag atomic. On a
    /// VST3 slot it is a parked handle (the VST3 monitor lands in Stage B) — stored harmlessly.
    monitor_gain: Arc<AtomicU32>,
    /// The ids `plugin_set_param` accepts. The SAME `Arc` the owner thread refills whenever it
    /// enumerates the plugin's params (`ParamIds`).
    param_ids: ParamIds,
}

impl SlotHandle {
    /// The one ordered teardown for a parked (or just-built-but-rejected) slot handle: stop → join
    /// the owner thread → `Close()` the WebView2 SharedBuffer on the UI thread. ORDER IS
    /// LOAD-BEARING and matches the documented host invariant. Setting `running=false` first lets
    /// the owner loop exit; JOINING it transitively guarantees the RT producer is dead (`owner_main`
    /// joins its RT child before returning), so after the join nothing still writes the SharedBuffer
    /// mapping via `shared_ptr` — which is what makes the `Close()` safe. `request_tx` is held until
    /// AFTER the join so the owner loop exits on `running`, not a Disconnected recv (the `..` drops
    /// `event_tx` — the RT Consumer then just sees an empty ring), then dropped.
    ///
    /// Deliberately NOT a `Drop` impl: the UI-thread `Close()` needs the `window` handle, which
    /// `Drop` (only `&mut self`) can't supply — every real drop site (unload + the two load
    /// race-loss paths) already routes through here with a window in hand.
    fn teardown(self, window: &tauri::WebviewWindow) {
        let SlotHandle {
            info,
            running,
            owner_join,
            shared_buf,
            request_tx,
            ..
        } = self;
        let slot = info.slot;
        let t = Instant::now();
        running.store(false, Relaxed);
        let _ = request_tx.send(OwnerRequest::Wake);
        let _ = owner_join.join();
        let join_ms = t.elapsed().as_millis();
        drop(request_tx);
        log::info!("[plugin_host] slot {slot} owner joined in {join_ms} ms");
        shared_buf.close(window, slot);
    }
}

/// The `Send` halves of the RT producer's rings. Handed to each RT spawn and handed BACK when the
/// loop exits, so a plugin-requested restart respawns the producer without re-threading the owner's
/// ends (the cpal input producer, the monitor consumer and the command-side event producer stay put).
struct RtRings {
    event_rx: Consumer<PluginEvent>, // P9.5 main→audio events (notes + params)
    in_rx: Consumer<f32>,            // P11.0 cpal→RT audio input (mono frames)
    mon_tx: Producer<f32>,           // P11.3 RT→cpal-out monitor (wet mono frames)
}

/// What the RT producer returns on exit: the `Stopped` processor (deactivate needs it), the rings,
/// and the block state it last reconciled to — a respawn continues at THAT block, not the load-time one.
struct RtExit {
    stopped: StoppedPluginAudioProcessor<LfHost>,
    rings: RtRings,
    period_frames: u32,
    block_config_gen: u32,
    /// The hop-1 drift the loop had learned (ppm); the respawn seeds its controller with it.
    drift_ppm: f64,
}

/// The per-load constants every RT spawn of this slot shares (all `Copy`; the owner keeps them).
#[derive(Clone, Copy)]
struct RtConfig {
    slot: u8,
    shared_ptr: usize,
    cap_frames: u32,
    out_channels: u32,
    in_channels: u32,
    max_frames: u32,
    sample_rate: f64, // C (ctx rate)
    device_rate: f64, // D (render rate)
}

impl RtConfig {
    /// The activation the plugin gets at load AND at every plugin-requested restart (same D, same
    /// max block): a restart re-runs the plugin's own `activate` with unchanged host terms.
    fn audio_configuration(&self) -> PluginAudioConfiguration {
        PluginAudioConfiguration {
            sample_rate: self.device_rate,
            min_frames_count: 1,
            max_frames_count: self.max_frames,
        }
    }
}

/// Spawn the RT producer for `cfg` with its own run flag (independent of the owner's `running`, so
/// the owner can stop and respawn it mid-load). On spawn failure the processor and rings are lost
/// with the closure — the caller must `try_deactivate` the instance.
fn spawn_rt(
    cfg: &RtConfig,
    stopped: StoppedPluginAudioProcessor<LfHost>,
    rings: RtRings,
    period_frames: u32,
    block_config_gen: u32,
    drift_ppm: f64,
    diag: Arc<ProducerDiag>,
) -> Result<RtJoinGuard<RtExit>, String> {
    let rt_run = Arc::new(AtomicBool::new(true));
    let run = rt_run.clone();
    let c = *cfg;
    let join = std::thread::Builder::new()
        .name(format!("lf-clap-rt-{}", cfg.slot))
        .spawn(move || -> RtExit {
            producer_loop(
                stopped,
                c.shared_ptr,
                c.cap_frames,
                diag,
                run,
                c.out_channels,
                c.in_channels, // P11.1: 0 ⇒ keep InputAudioBuffers::empty() (synth)
                period_frames,
                block_config_gen,
                c.max_frames,
                c.sample_rate,
                c.device_rate,
                drift_ppm,
                rings,
            )
        })
        .map_err(|e| format!("failed to spawn RT thread: {e}"))?;
    Ok(RtJoinGuard::new(rt_run, join))
}

/// Report a load only once its RT producer runs (audit B1). `spawn` starts the producer; only
/// then does `ready_tx` carry `Ok(payload)`. A failed spawn hands the payload to `release` (the
/// owner closes the already-posted SharedBuffer there) and then sends the error, so the load
/// command answers `Err` instead of parking a slot that never makes a sound. A load command that
/// already gave up (its 15 s timeout) cannot park or tear down what arrives late, so a refused
/// send stops the producer (dropping `G` stops and joins it: `RtJoinGuard`) BEFORE `release`, and
/// nothing writes the mapping when it closes. `ready_tx` is a rendezvous channel
/// (`load_ready_channel`), so a send that succeeds was received. The caller still owns the rest of
/// the undo (deactivate, module teardown) on `None`. Shared by the CLAP and VST3 owners.
pub(super) fn spawn_rt_then_ready<G, P>(
    ready_tx: &std::sync::mpsc::SyncSender<Result<P, String>>,
    payload: P,
    spawn: impl FnOnce() -> Result<G, String>,
    release: impl FnOnce(P),
) -> Option<G> {
    match spawn() {
        Ok(guard) => match ready_tx.send(Ok(payload)) {
            Ok(()) => Some(guard),
            Err(std::sync::mpsc::SendError(late)) => {
                log::warn!("[plugin_host] the load finished after its command timed out; stopping the producer and closing its shared buffer");
                drop(guard);
                if let Ok(payload) = late {
                    release(payload);
                }
                None
            }
        },
        Err(e) => {
            log::error!("[plugin_host] {e}");
            release(payload);
            let _ = ready_tx.send(Err(e));
            None
        }
    }
}

/// The owner → load command channel of `load` and `vst3_load`. Rendezvous (capacity 0): a send
/// that succeeds was received, so no result can sit unread in the channel when the command's 15 s
/// timeout drops the receiver; a late one goes back to the owner, which undoes it
/// (`spawn_rt_then_ready`). With a buffer, a result sent between that timeout and the drop would
/// sit in it and leave with the channel, its SharedBuffer never `Close()`d.
fn load_ready_channel<T>() -> (std::sync::mpsc::SyncSender<T>, std::sync::mpsc::Receiver<T>) {
    std::sync::mpsc::sync_channel(0)
}

/// Plugin-requested restart (CLAP `host.request_restart`), on the owner thread: stop + join the RT
/// producer (it hands back the `Stopped` processor and the rings), `deactivate`, `activate` again at
/// the same configuration, respawn. The editor and the native cpal streams stay up; the audible gap
/// is the join + the plugin's activate (a few blocks; the monitor ring starves to silence meanwhile,
/// the input ring is flushed on respawn like any re-arm). A failed re-activation leaves the slot
/// SILENT but still serviced by the owner loop, so unload/reload work normally — the error is logged.
fn service_restart(
    instance: &mut PluginInstance<LfHost>,
    rt_guard: &mut Option<RtJoinGuard<RtExit>>,
    cfg: &RtConfig,
    diag: &Arc<ProducerDiag>,
) -> Result<(), String> {
    let guard = rt_guard
        .take()
        .ok_or_else(|| "no RT producer to restart (an earlier restart failed)".to_string())?;
    let exit = match guard.stop_and_join() {
        Ok(exit) => exit,
        Err(_) => {
            // The unwound thread dropped its processor handle, so the instance can still deactivate.
            let _ = instance.try_deactivate();
            return Err("RT thread panicked during the restart join".to_string());
        }
    };
    instance.deactivate(exit.stopped);
    let stopped = instance
        .activate(|_, _| (), cfg.audio_configuration())
        .map_err(|e| format!("re-activate failed: {e}"))?;
    match spawn_rt(
        cfg,
        stopped,
        exit.rings,
        exit.period_frames,
        exit.block_config_gen,
        exit.drift_ppm,
        diag.clone(),
    ) {
        Ok(guard) => {
            *rt_guard = Some(guard);
            Ok(())
        }
        Err(e) => {
            let _ = instance.try_deactivate();
            Err(e)
        }
    }
}

/// Owner-thread guard for one RT producer. A plain dropped `JoinHandle` detaches; this guard stops
/// and joins instead, including while the owner stack is unwinding from a panic. Declare it after
/// the plugin/module locals so it drops first and no RT code can outlive their DLL-backed objects.
struct RtJoinGuard<T> {
    running: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<T>>,
}

impl<T> RtJoinGuard<T> {
    fn new(running: Arc<AtomicBool>, join: std::thread::JoinHandle<T>) -> Self {
        Self {
            running,
            join: Some(join),
        }
    }

    fn stop_and_join(mut self) -> std::thread::Result<T> {
        self.running.store(false, Release);
        self.join.take().expect("RT join handle already taken").join()
    }
}

impl<T> Drop for RtJoinGuard<T> {
    fn drop(&mut self) {
        self.running.store(false, Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Begin one WebView document's native-host session. A reload keeps fully loaded slots for the
/// normal `list_loaded` → `unload` recovery, but cancels and releases reservations that the old
/// document left mid-setup. The old load command still owns its thread and will tear down any late
/// result when its reservation check fails.
pub fn begin_frontend_session(state: &PluginHostState) -> Result<u32, String> {
    let mut slots = state.slots.lock().map_err(|_| "slots lock poisoned")?;
    let next = state.frontend_epoch.load(Relaxed).wrapping_add(1).max(1);
    state.frontend_epoch.store(next, Relaxed);
    let mut cancelled = 0;
    for slot in slots.iter_mut() {
        if let SlotState::Loading { running, .. } = slot {
            running.store(false, Release);
            *slot = SlotState::Empty;
            cancelled += 1;
        }
    }
    if cancelled > 0 {
        log::info!(
            "[plugin_host] frontend epoch {next} cancelled {cancelled} in-flight load(s)"
        );
    }
    Ok(next)
}

/// Atomically validate the calling document and reserve its slot before foreign setup begins.
fn reserve_load(
    state: &PluginHostState,
    slot: u8,
    frontend_epoch: u32,
    running: &Arc<AtomicBool>,
) -> Result<u32, String> {
    let mut slots = state.slots.lock().map_err(|_| "slots lock poisoned")?;
    let current_epoch = state.frontend_epoch.load(Relaxed);
    if frontend_epoch != current_epoch {
        return Err(format!(
            "stale frontend epoch {frontend_epoch} (current {current_epoch})"
        ));
    }
    match &slots[slot as usize] {
        SlotState::Empty => {}
        SlotState::Loading { .. } => return Err(format!("slot {slot} is already loading a plugin")),
        SlotState::Loaded(_) => return Err(format!("slot {slot} already has a plugin loaded")),
    }
    let load_gen = LOAD_GEN.fetch_add(1, Relaxed).wrapping_add(1);
    slots[slot as usize] = SlotState::Loading {
        load_gen,
        frontend_epoch,
        running: running.clone(),
    };
    Ok(load_gen)
}

/// Clear only this load's reservation. A newer document/load may already own the slot.
fn clear_load_reservation(state: &PluginHostState, slot: u8, load_gen: u32) {
    if let Ok(mut slots) = state.slots.lock() {
        if matches!(
            &slots[slot as usize],
            SlotState::Loading { load_gen: current, .. } if *current == load_gen
        ) {
            slots[slot as usize] = SlotState::Empty;
        }
    }
}

/// Park a completed setup only if it still owns the reservation. Every losing path tears the live
/// handle down outside the slots lock, including lock poisoning, so no owner thread is detached.
fn park_loaded(
    state: &PluginHostState,
    window: &tauri::WebviewWindow,
    slot: u8,
    load_gen: u32,
    frontend_epoch: u32,
    handle: SlotHandle,
) -> Result<PluginInfo, String> {
    let info = handle.info.clone();
    let mut slots = match state.slots.lock() {
        Ok(slots) => slots,
        Err(_) => {
            handle.teardown(window);
            return Err("slots lock poisoned".to_string());
        }
    };
    if slots[slot as usize].owns_load(load_gen, frontend_epoch) {
        slots[slot as usize] = SlotState::Loaded(handle);
        Ok(info)
    } else {
        drop(slots);
        handle.teardown(window);
        Err(format!(
            "plugin load for slot {slot} was superseded by a frontend reload"
        ))
    }
}

#[cfg(test)]
mod host_lifecycle_tests {
    use super::*;

    #[test]
    fn frontend_reload_cancels_old_load_without_clobbering_the_next_one() {
        let state = PluginHostState::default();
        let epoch_1 = begin_frontend_session(&state).unwrap();
        let old_running = Arc::new(AtomicBool::new(true));
        let old_gen = reserve_load(&state, 0, epoch_1, &old_running).unwrap();

        let epoch_2 = begin_frontend_session(&state).unwrap();
        assert_ne!(epoch_1, epoch_2);
        assert!(!old_running.load(Acquire));

        let stale_running = Arc::new(AtomicBool::new(true));
        assert!(reserve_load(&state, 0, epoch_1, &stale_running).is_err());

        let new_running = Arc::new(AtomicBool::new(true));
        let new_gen = reserve_load(&state, 0, epoch_2, &new_running).unwrap();
        clear_load_reservation(&state, 0, old_gen);
        assert!(state.slots.lock().unwrap()[0].owns_load(new_gen, epoch_2));

        clear_load_reservation(&state, 0, new_gen);
        assert!(matches!(state.slots.lock().unwrap()[0], SlotState::Empty));
    }

    /// Audit B1: the owner reports a load only after the RT producer spawned. A failed spawn gives
    /// the payload (the posted SharedBuffer) back for release and the load command sees `Err`.
    #[test]
    fn a_failed_rt_spawn_fails_the_load() {
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<&str, String>>(1);
        let mut released = None;
        let guard: Option<()> = spawn_rt_then_ready(
            &ready_tx,
            "shared buffer",
            || Err("failed to spawn RT thread: injected".to_string()),
            |payload| released = Some(payload),
        );
        assert!(guard.is_none());
        assert_eq!(released, Some("shared buffer"), "the posted buffer is released");
        assert_eq!(
            ready_rx.try_recv().unwrap(),
            Err("failed to spawn RT thread: injected".to_string())
        );

        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<&str, String>>(1);
        let guard = spawn_rt_then_ready(
            &ready_tx,
            "shared buffer",
            || {
                // Nothing may be reported before the producer exists.
                assert!(ready_rx.try_recv().is_err());
                Ok(7)
            },
            |_| panic!("a spawned producer must not release the buffer"),
        );
        assert_eq!(guard, Some(7));
        assert_eq!(ready_rx.try_recv().unwrap(), Ok("shared buffer"));
    }

    /// A load that finishes after its command timed out (the receiver is gone): nobody will park
    /// or tear it down, so the owner stops the producer it just spawned, THEN releases the posted
    /// buffer, and takes its undo path.
    #[test]
    fn a_load_that_finishes_after_its_command_timed_out_is_undone_by_the_owner() {
        struct StopOnDrop<'a>(&'a AtomicBool);
        impl Drop for StopOnDrop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Release);
            }
        }
        let stopped = AtomicBool::new(false);
        let (ready_tx, ready_rx) = load_ready_channel::<Result<&str, String>>();
        drop(ready_rx);
        let mut released = None;
        let guard = spawn_rt_then_ready(
            &ready_tx,
            "shared buffer",
            || Ok(StopOnDrop(&stopped)),
            |payload| {
                assert!(
                    stopped.load(Acquire),
                    "the producer must be stopped before its buffer closes"
                );
                released = Some(payload);
            },
        );
        assert!(guard.is_none(), "the owner must take its undo path");
        assert_eq!(released, Some("shared buffer"), "the posted buffer is released");
    }

    /// The window the rendezvous closes: the load command's receiver still exists but no longer
    /// receives (between its 15 s timeout and its drop). The load channel must refuse a result
    /// then, so it goes back to the owner instead of leaving with the channel.
    #[test]
    fn the_load_channel_holds_no_result_its_command_is_not_receiving() {
        let (ready_tx, ready_rx) = load_ready_channel::<Result<&str, String>>();
        assert!(
            matches!(
                ready_tx.try_send(Ok("shared buffer")),
                Err(std::sync::mpsc::TrySendError::Full(Ok("shared buffer")))
            ),
            "a result the command is not receiving must be refused, not buffered"
        );
        assert!(ready_rx.try_recv().is_err(), "nothing sits unread in the channel");
    }

    #[test]
    fn rt_join_guard_stops_and_joins_on_drop() {
        let running = Arc::new(AtomicBool::new(true));
        let worker_running = running.clone();
        let exited = Arc::new(AtomicBool::new(false));
        let worker_exited = exited.clone();
        let join = std::thread::spawn(move || {
            while worker_running.load(Acquire) {
                std::thread::yield_now();
            }
            worker_exited.store(true, Release);
        });

        drop(RtJoinGuard::new(running, join));
        assert!(exited.load(Acquire));
    }
}

/// The command-side checks of `plugin_note_on/off` and `plugin_set_param` against a parked slot
/// with a small event ring and no plugin behind it: the ring's consumer and the owner channel are
/// held by the test, so what reaches the processor and the VST3 controller is observable.
#[cfg(test)]
mod command_boundary_tests {
    use super::*;

    const LISTED: u32 = 825_615_485; // hash-like, as Surge's are

    struct Fixture {
        state: PluginHostState,
        events: Consumer<PluginEvent>,
        requests: std::sync::mpsc::Receiver<OwnerRequest>,
        diag: Arc<ProducerDiag>,
    }

    fn loaded_slot(format: &str, ring_cap: usize) -> Fixture {
        let (event_tx, events) = RingBuffer::<PluginEvent>::new(ring_cap);
        let (request_tx, requests) = std::sync::mpsc::channel();
        let diag = Arc::new(ProducerDiag::new());
        let param_ids = ParamIds::default();
        publish_param_ids(
            &param_ids,
            &[ParamDesc {
                id: LISTED,
                name: "Cutoff".to_string(),
                min_value: 0.0,
                max_value: 1.0,
                default_value: 0.5,
                value: 0.5,
            }],
        );
        let handle = SlotHandle {
            info: PluginInfo {
                slot: 0,
                descriptor: PluginDescriptor {
                    id: "fixture".to_string(),
                    name: "Fixture".to_string(),
                    format: format.to_string(),
                    path: String::new(),
                    is_effect: None,
                },
            },
            running: Arc::new(AtomicBool::new(true)),
            diag: diag.clone(),
            owner_join: std::thread::spawn(|| {}),
            shared_buf: SharedBufferHandle::detached_for_test(),
            event_tx: Arc::new(std::sync::Mutex::new(event_tx)),
            request_tx,
            monitor_gain: Arc::new(AtomicU32::new(1.0f32.to_bits())),
            param_ids,
        };
        let state = PluginHostState::default();
        state.slots.lock().unwrap()[0] = SlotState::Loaded(handle);
        Fixture {
            state,
            events,
            requests,
            diag,
        }
    }

    /// Audit B2: an id the plugin never listed is refused before it reaches the ring.
    #[test]
    fn set_param_refuses_an_id_the_plugin_never_listed() {
        let mut f = loaded_slot("clap", 8);
        assert!(set_param(&f.state, 0, LISTED + 1, 0.3).is_err());
        assert!(f.events.pop().is_err(), "an unknown id must not reach the ring");

        assert!(set_param(&f.state, 0, LISTED, 0.3).is_ok());
        assert!(matches!(
            f.events.pop(),
            Ok(PluginEvent::Param { id: LISTED, .. })
        ));
    }

    /// Audit B3: a full ring makes a note-off an `Err` (it would otherwise stick silently).
    #[test]
    fn a_note_off_into_a_full_ring_is_an_error() {
        let f = loaded_slot("clap", 2);
        for key in [60, 62] {
            enqueue_event(&f.state, 0, PluginEvent::NoteOn { key, velocity: 1.0 }).unwrap();
        }
        let err = enqueue_event(&f.state, 0, PluginEvent::NoteOff { key: 60 }).unwrap_err();
        assert!(err.contains("note-off 60"), "the error names the lost event: {err}");
        assert_eq!(f.diag.events_dropped.load(Relaxed), 1);
    }

    /// Audit B4: a VST3 set whose ring push failed is not mirrored to the edit controller, so
    /// the plugin's GUI keeps the value its processor actually has.
    #[test]
    fn vst3_set_param_mirrors_to_the_controller_only_after_a_successful_push() {
        let f = loaded_slot("vst3", 1);
        set_param(&f.state, 0, LISTED, 0.25).unwrap();
        assert!(matches!(
            f.requests.try_recv(),
            Ok(OwnerRequest::SetParamNormalized(LISTED, v)) if v == 0.25
        ));

        assert!(set_param(&f.state, 0, LISTED, 0.75).is_err(), "the ring is full");
        assert!(
            f.requests.try_recv().is_err(),
            "the controller must not get a value the processor never got"
        );
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
/// producer keeps `InputAudioBuffers::empty()` and arming returns "no input bus". Never `.max(1)`:
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

// ---- the gate emitter (owner thread, ~2s) --------------------------------------------------

/// Owner-thread-local gate state: window-delta baselines (single reader/writer → plain fields, no
/// atomics or false sharing), the drift EWMA, and the run clock. One `GateState` per load, so a
/// `reloadPlugin` (new owner thread) starts fresh; the `load_gen` guard carves the first emit out
/// as a warmup line so a poisoned cross-reload delta is never reported (M4).
#[cfg(debug_assertions)]
struct GateState {
    started: Instant,
    last_load_gen: u32,
    drift_ewma: Option<f64>,
    underruns_b: u32,
    js_dropped_b: u32,
    frames_dropped_b: u64,
    events_dropped_b: u64,
    input_starves_b: u64,
    input_overruns_b: u64,
    monitor_starves_b: u64,
    monitor_overruns_b: u64,
    pace_late_b: u64,
    pace_reanchors_b: u64,
}

#[cfg(debug_assertions)]
impl GateState {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            last_load_gen: 0,
            drift_ewma: None,
            underruns_b: 0,
            js_dropped_b: 0,
            frames_dropped_b: 0,
            events_dropped_b: 0,
            input_starves_b: 0,
            input_overruns_b: 0,
            monitor_starves_b: 0,
            monitor_overruns_b: 0,
            pace_late_b: 0,
            pace_reanchors_b: 0,
        }
    }
}

/// EWMA weight on the latest sample (~5-window memory) so a single GC-stall spike doesn't poison
/// the reported drift.
#[cfg(debug_assertions)]
const DRIFT_EWMA_ALPHA: f64 = 0.3;

/// One grep-able `[diag] {"step":"gate",...}` line. Window deltas (`underruns`/`overruns`/
/// `js_dropped`) are advanced against owner-local baselines; absolutes (`frames_written`/
/// `consumed`) prove liveness; `fill_pct` is against the CONTROL BAND `[0, 2·target]` so target =
/// 50%; `drift_ppm` is the EWMA of the integral, `drift_ppm_inst` the raw current integral.
/// `ratio_spread` is the per-window min/max range the RT thread tracked (B3 loop-engaged proof).
#[cfg(debug_assertions)]
fn emit_gate(diag: &ProducerDiag, gs: &mut GateState) {
    let load_gen = diag.load_gen.load(Relaxed);
    // M4: first emit after a (re)load — seed baselines from the fresh counters, skip the poisoned
    // window delta, and mark the line warmup so the gate consumer doesn't score it.
    if load_gen != gs.last_load_gen {
        gs.last_load_gen = load_gen;
        gs.underruns_b = diag.underruns.load(Relaxed);
        gs.js_dropped_b = diag.js_dropped.load(Relaxed);
        gs.frames_dropped_b = diag.frames_dropped.load(Relaxed);
        gs.events_dropped_b = diag.events_dropped.load(Relaxed);
        gs.input_starves_b = diag.input_starves.load(Relaxed);
        gs.input_overruns_b = diag.input_overruns.load(Relaxed);
        gs.monitor_starves_b = diag.monitor_starves.load(Relaxed);
        gs.monitor_overruns_b = diag.monitor_overruns.load(Relaxed);
        gs.pace_late_b = diag.pace_late.load(Relaxed);
        gs.pace_reanchors_b = diag.pace_reanchors.load(Relaxed);
        diag.pace_late_max_us.store(0, Relaxed);
        // NB: out_peak is NOT reset here. Each load gets a fresh ProducerDiag (out_peak=0 at
        // construction), so the startup drone's peak accumulates and is reported by the first
        // real emit (a windowed swap) rather than being wiped by this warmup line.
        gs.drift_ewma = None;
        println!("[diag] {}", serde_json::json!({"step":"gate","warmup":true,"load_gen":load_gen}));
        return;
    }

    // Window deltas (advance baselines). wrapping_sub tolerates the u32 frame-counter wrap.
    let underruns_now = diag.underruns.load(Relaxed);
    let underruns = underruns_now.wrapping_sub(gs.underruns_b);
    gs.underruns_b = underruns_now;
    let js_dropped_now = diag.js_dropped.load(Relaxed);
    let js_dropped = js_dropped_now.wrapping_sub(gs.js_dropped_b);
    gs.js_dropped_b = js_dropped_now;
    let frames_dropped_now = diag.frames_dropped.load(Relaxed);
    let overruns = frames_dropped_now.wrapping_sub(gs.frames_dropped_b);
    gs.frames_dropped_b = frames_dropped_now;
    let events_dropped_now = diag.events_dropped.load(Relaxed);
    let events_dropped = events_dropped_now.wrapping_sub(gs.events_dropped_b);
    gs.events_dropped_b = events_dropped_now;
    // P11.0 input ring: windowed starve + overrun counts and a bare fill snapshot (unread mono
    // frames). Starve = the RT block went short; overrun = the cpal capture side dropped frames into
    // a full ring (the opposite lag, invisible as a starve). Both stay 0 on a synth slot / when
    // disarmed (nothing touches them without an input bus).
    let input_starves_now = diag.input_starves.load(Relaxed);
    let input_starves = input_starves_now.wrapping_sub(gs.input_starves_b);
    gs.input_starves_b = input_starves_now;
    let input_overruns_now = diag.input_overruns.load(Relaxed);
    let input_overruns = input_overruns_now.wrapping_sub(gs.input_overruns_b);
    gs.input_overruns_b = input_overruns_now;
    let input_fill = diag.input_fill.load(Relaxed);
    let input_fill_max = diag.input_fill_max.swap(0, Relaxed);
    let input_rate = diag.input_rate.load(Relaxed);
    let input_drift_ppm = f64::from_bits(diag.input_drift_ppm_bits.load(Relaxed));
    // P11.3 monitor: windowed starve + overrun counts and fill/rate/drift snapshots (0 on a slot with
    // no native monitor armed — nothing touches them until the monitor is engaged). Overrun = wet
    // frames the publish dropped into a full mon ring, which the cpal-out side can never report.
    let monitor_starves_now = diag.monitor_starves.load(Relaxed);
    let monitor_starves = monitor_starves_now.wrapping_sub(gs.monitor_starves_b);
    gs.monitor_starves_b = monitor_starves_now;
    let monitor_overruns_now = diag.monitor_overruns.load(Relaxed);
    let monitor_overruns = monitor_overruns_now.wrapping_sub(gs.monitor_overruns_b);
    gs.monitor_overruns_b = monitor_overruns_now;
    let monitor_fill = diag.monitor_fill.load(Relaxed);
    let monitor_rate = diag.monitor_rate.load(Relaxed);
    let monitor_out_block = diag.monitor_out_block.load(Relaxed);
    let monitor_drift_ppm = f64::from_bits(diag.monitor_drift_ppm_bits.load(Relaxed));
    // out_peak: max |mono| over the window; swap-reset so each line reports its own window.
    // >0 once a routed note voices — the headless note-routing proof (P9.5).
    let out_peak = f32::from_bits(diag.out_peak_bits.swap(0, Relaxed));
    // Pacing: blocks that ended past their deadline, and stalls long enough to lose periods for good.
    let pace_late_now = diag.pace_late.load(Relaxed);
    let pace_late = pace_late_now.wrapping_sub(gs.pace_late_b);
    gs.pace_late_b = pace_late_now;
    let pace_reanchors_now = diag.pace_reanchors.load(Relaxed);
    let pace_reanchors = pace_reanchors_now.wrapping_sub(gs.pace_reanchors_b);
    gs.pace_reanchors_b = pace_reanchors_now;
    let pace_late_max_us = diag.pace_late_max_us.swap(0, Relaxed);

    // Control-band fill_pct: map hop2_fill onto [0, 2·target] so the setpoint reads as 50%.
    let hop2_fill = diag.hop2_fill.load(Relaxed);
    let target = diag.target_frames.load(Relaxed);
    let fill_pct = if target > 0 { (hop2_fill as u64 * 50 / target as u64) as u32 } else { 0 };

    // ratio_spread: the per-window min/max the RT thread accumulated. Read, then reset the window
    // by seeding both bounds to the current ratio (benign owner↔RT race on a relaxed diag).
    let ratio = f64::from_bits(diag.resample_ratio_bits.load(Relaxed));
    let rmin = f64::from_bits(diag.ratio_min_bits.load(Relaxed));
    let rmax = f64::from_bits(diag.ratio_max_bits.load(Relaxed));
    let ratio_spread = if rmax >= rmin { rmax - rmin } else { 0.0 };
    diag.ratio_min_bits.store(ratio.to_bits(), Relaxed);
    diag.ratio_max_bits.store(ratio.to_bits(), Relaxed);

    // drift: instantaneous integral + EWMA (a GC stall spikes inst, not the smoothed value).
    let drift_inst = f64::from_bits(diag.drift_ppm_bits.load(Relaxed));
    let ewma = match gs.drift_ewma {
        Some(prev) => DRIFT_EWMA_ALPHA * drift_inst + (1.0 - DRIFT_EWMA_ALPHA) * prev,
        None => drift_inst,
    };
    gs.drift_ewma = Some(ewma);

    let cap = diag.ring_capacity.load(Relaxed);
    let used = diag.ring_used.load(Relaxed);
    let hop1_fill_pct = if cap > 0 { used * 100 / cap } else { 0 };

    let mut line = serde_json::json!({
        "step": "gate",
        "t_s": gs.started.elapsed().as_secs(),
        "drift_ppm": ewma.round() as i64,
        "drift_ppm_inst": drift_inst.round() as i64,
        "fill_pct": fill_pct,
        "hop2_fill": hop2_fill,
        "target_frames": target,
        "underruns": underruns,
        "overruns": overruns,
        "js_dropped": js_dropped,
        "resample_ratio": ratio,
        "ratio_spread": ratio_spread,
        "ratio_clamp_fails": diag.ratio_clamp_fails.load(Relaxed),
        "rt_faults": diag.rt_faults.load(Relaxed),
        "out_peak": out_peak,
        "events_dropped": events_dropped,
        "input_fill": input_fill,
        "input_starves": input_starves,
        "input_overruns": input_overruns, // capture frames dropped into a FULL input ring
        "input_rate": input_rate,
        "input_drift_ppm": input_drift_ppm.round() as i64,
        "monitor_starves": monitor_starves,
        "monitor_overruns": monitor_overruns, // wet frames dropped into a FULL mon ring
        "monitor_fill": monitor_fill,
        "monitor_rate": monitor_rate,
        "monitor_out_block": monitor_out_block, // callback period, startup fallback only
        "monitor_output_latency_ms": diag.monitor_output_latency_ns.load(Relaxed) as f64 / 1e6,
        "monitor_output_latency_source": if diag.monitor_output_latency_ns.load(Relaxed) > 0 { "reported" } else { "callback-period-fallback" },
        "monitor_drift_ppm": monitor_drift_ppm.round() as i64,
        "rt_allocs": super::rt_alloc::RT_ALLOCS.load(Relaxed),
        "alive": diag.alive.load(Relaxed),
        "frames_written": diag.frames_written.load(Relaxed),
        "consumed": diag.consumed.load(Relaxed),
        "hop1_fill_pct": hop1_fill_pct,
        "sample_rate": f64::from_bits(diag.sample_rate.load(Relaxed)),
        "device_rate": f64::from_bits(diag.device_rate_bits.load(Relaxed)),
        "max_frames": diag.max_frames.load(Relaxed),
        "block_frames": diag.block_frames.load(Relaxed), // P11.3: the active RT block — confirms a buffer pick
        "out_channels": diag.out_channels.load(Relaxed),
        "load_gen": load_gen,
    });
    // Added after the macro: one more field inside it passes serde_json's recursion limit.
    line["pace_late"] = pace_late.into();
    line["pace_reanchors"] = pace_reanchors.into();
    line["pace_late_max_us"] = pace_late_max_us.into();
    line["input_fill_max"] = input_fill_max.into();
    line["monitor_pads"] = diag.monitor_pads.load(Relaxed).into(); // cumulative
    println!("[diag] {line}");
}

/// Everything the owner thread carries out of setup. `_entry` is held only to keep the loaded
/// bundle alive for the instance's lifetime (a dropped entry would unload the dylib).
struct OwnerSetup {
    _entry: PluginEntry,
    instance: PluginInstance<LfHost>,
    stopped: StoppedPluginAudioProcessor<LfHost>,
    shared_ptr: usize,
    cap_frames: u32,
    shared_buf: SharedBufferHandle,
    out_channels: u32,
    in_channels: u32, // P11.1: plugin audio-input port-0 channel count; 0 = no input bus (synth)
    period_frames: u32,
    max_frames: u32,
    device_rate: f64, // D — the rate the plugin renders at (resampled to C downstream)
    name: String,
}

/// The owner thread = clack's "main thread" for this slot. It creates + activates the `!Send`
/// instance, provisions the hop-1 SharedBuffer (on the UI thread), spawns the RT producer
/// (handing it the Send `Stopped` + the mapped ring pointer), emits the gate every ~2s, and on
/// shutdown joins the RT thread (recovering `Stopped`) to `deactivate` here.
#[allow(clippy::too_many_arguments)]
fn owner_main(
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
    // P11.3 native-monitor output gain (linear). Created in `load` (so the SlotHandle holds the
    // SAME Arc and `set_monitor_gain` can store to it without an owner hop) and handed here to the
    // cpal OUTPUT callback. Read atomically each callback — no stream rebuild on a slider change.
    monitor_gain: Arc<AtomicU32>,
    // The known param ids (`ParamIds`): filled here before the load is reported, refreshed on
    // every enumeration; the SlotHandle holds the same Arc for `set_param`'s check.
    param_ids: ParamIds,
) {
    // P10.0: the plugin→host "editor closed" signal, shared with the instance's Shared handler
    // (set by `HostGuiImpl::closed`) and polled by the owner loop below.
    let editor_closed = Arc::new(EditorClosed::default());
    // The hosted editor's window, for the plugin's `request_resize` (0 while none is open).
    let hosted_hwnd = Arc::new(AtomicIsize::new(0));
    // Capture before setup reads CHOSEN_BLOCK_FRAMES. Any setting change during the foreign-plugin
    // setup window leaves a generation mismatch for the RT loop to reconcile.
    let block_config_gen_at_load = BLOCK_CONFIG_GEN.load(Acquire);
    let setup = (|| -> Result<OwnerSetup, String> {
        // SAFETY: PluginEntry::load runs the bundle's foreign CLAP entry-init code. P9.1 scans
        // out-of-process to survive crashy bundles; here Surge XT is already vetted by the scan.
        let entry = unsafe { PluginEntry::load(&path) }.map_err(|e| format!("load failed: {e}"))?;
        let id_c = CString::new(id.clone()).map_err(|e| format!("bad id: {e}"))?;
        let name = {
            let factory = entry
                .get_plugin_factory()
                .ok_or_else(|| "no plugin factory".to_string())?;
            factory
                .plugin_descriptors()
                .find(|d| d.id() == Some(id_c.as_c_str()))
                .and_then(|d| d.name())
                .map(|c| c.to_string_lossy().into_owned())
                .unwrap_or_else(|| id.clone())
        };
        let host_info =
            HostInfo::new("BleepLoop", "BleepLoop", "https://bleeploop.local", "0.1.0")
                .map_err(|e| format!("host info: {e}"))?;
        let ec_for_shared = editor_closed.clone();
        let hh_for_shared = hosted_hwnd.clone();
        let mut instance = PluginInstance::<LfHost>::new(
            move |_| LfShared {
                editor_closed: ec_for_shared,
                hosted_hwnd: hh_for_shared,
                callback_requested: AtomicBool::new(false),
                restart_requested: AtomicBool::new(false),
            },
            |_| LfMain::default(),
            &entry,
            id_c.as_c_str(),
            &host_info,
        )
        .map_err(|e| format!("instantiate failed: {e}"))?;

        // init() may request a callback, but only the fully initialized instance can receive it.
        deliver_plugin_callback(&mut instance);

        let out_channels = query_out_channels(&mut instance)?;
        // P11.1: query the input bus too (0 for a synth → the producer keeps `empty()`).
        let in_channels = query_in_channels(&mut instance)?;

        // P9.4 forced-mismatch split (M5): C = ctx rate (what the Web Audio clock + worklet run
        // at), D = the device rate we activate + render the plugin at. With FORCE_DEVICE_RATE set
        // they differ, so rubato does a real D→C conversion and the gate isn't a false pass (B3).
        let c = sample_rate;
        let d = force_device_rate().unwrap_or(c);

        // Period + activation are at D (the render rate); the WASAPI query is for D's frame count.
        let wasapi_default = wasapi_period_frames(d).unwrap_or_else(|e| {
            log::warn!("[plugin_host] WASAPI period query failed ({e}); 10ms fallback");
            ((d * 0.01).round() as u32).max(64)
        });
        // P11.3 live buffer: an explicit user pick (CHOSEN_BLOCK_FRAMES != 0) wins; else the device
        // default. The ASIO 256 cap applies only to the DEFAULT — an explicit pick is honored up to
        // MAX_SELECTABLE_BLOCK, so the buffer dropdown can go above 256 on ASIO too. (Block_dt-
        // normalized DriftController → stable at any block.)
        let chosen = CHOSEN_BLOCK_FRAMES.load(Relaxed);
        let mut period_frames = if chosen != 0 { chosen } else { wasapi_default };
        // Cap the DEFAULT block to the ~256-frame ASIO buffer whenever an ASIO device is AVAILABLE
        // (not merely when the tier is currently enabled). asio_available() is a startup-fixed
        // OnceLock, so this LOAD-time cap can never disagree with the ARM-time ring setpoint the way a
        // runtime use_asio() read would (the load-vs-arm skew the review flagged). Safe for the WASAPI
        // fallback too: a 256 producer block on a ~441-frame WASAPI device is just finer, lower-latency
        // chunks — the ring + DriftController decouple it, and both setpoints (0.015/0.020) sit well
        // above 256. Explicit buffer picks bypass the cap.
        if chosen == 0 && crate::audio_output::asio_available() {
            period_frames = period_frames.min(ASIO_MAX_BLOCK_FRAMES);
        }
        let period_frames = period_frames.min(MAX_SELECTABLE_BLOCK);
        // Roomy activation: size buffers + max_frames_count to the SELECTABLE max + headroom, so the
        // live block can sweep up to MAX_SELECTABLE_BLOCK without re-activating the plugin (Task 10).
        let max_frames = MAX_SELECTABLE_BLOCK + 128;

        let cap_frames = HOP1_CAPACITY_FRAMES;
        // Controller setpoint in C-frames (hop-2 fill is measured at C — the worklet pops at C).
        let target_frames = (TARGET_FILL_SECONDS * c).round() as u32;
        diag.init(
            c,
            d,
            max_frames,
            out_channels,
            cap_frames as usize,
            target_frames,
            load_gen,
        );
        diag.block_frames.store(period_frames, Relaxed); // P11.3: the active RT block (gate reads it)

        // Activate at D — the plugin renders D-frame blocks; rubato resamples them to C downstream.
        // (Surge XT activate(48000) with no device at 48k is untested — spec §9; fall back to a
        // different D ≠ C if it rejects.)
        let cfg = PluginAudioConfiguration {
            sample_rate: d,
            min_frames_count: 1,
            max_frames_count: max_frames,
        };
        let stopped = instance
            .activate(|_, _| (), cfg)
            .map_err(|e| format!("activate failed: {e}"))?;

        if !running.load(Acquire) {
            instance.deactivate(stopped);
            return Err("plugin load cancelled by frontend reload".to_string());
        }

        // Post the SharedBuffer only after activation succeeds. meta.sampleRate = C because hop-1
        // frames are post-resample; the JS drain's maxLagFrames must be C-based, not D-based.
        let (shared_ptr, shared_buf) =
            match create_shared_ring(
                &window,
                cap_frames,
                slot,
                frontend_epoch,
                load_token,
                &running,
                c,
                in_channels,
            ) {
                Ok(ring) => ring,
                Err(e) => {
                    instance.deactivate(stopped);
                    return Err(e);
                }
            };

        Ok(OwnerSetup {
            _entry: entry,
            instance,
            stopped,
            shared_ptr,
            cap_frames,
            shared_buf,
            out_channels,
            in_channels,
            period_frames,
            max_frames,
            device_rate: d,
            name,
        })
    })();

    let OwnerSetup {
        _entry,
        mut instance,
        stopped,
        shared_ptr,
        cap_frames,
        shared_buf,
        out_channels,
        in_channels,
        period_frames,
        max_frames,
        device_rate,
        name,
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
            format: "clap".to_string(),
            path,
            // The load-response descriptor isn't used for gain staging (JS keeps the scan
            // descriptor with the real category); leave it unclassified.
            is_effect: None,
        },
    };
    // Before the load is reported, so the frontend's first `setParameter` finds its ids.
    refresh_clap_param_ids(&mut instance, &param_ids);

    // P11.0: the cpal→RT audio-input ring. Built owner-thread-local: the Consumer crosses to the
    // RT producer (like event_rx); the Producer stays owner-local behind an Arc<Mutex> so
    // successive cpal streams (arm/disarm/re-arm) feed the one SPSC ring without re-threading the
    // Consumer into the already-spawned RT loop. The ring is ALWAYS present so the RT loop drains
    // uniformly (empty ⇒ zero-fill ⇒ silence into the plugin's input bus when disarmed).
    let (in_tx, in_rx) = RingBuffer::<f32>::new(IN_RING_CAP);
    let input_producer = Arc::new(std::sync::Mutex::new(in_tx));
    // P11.3: the RT→cpal-out monitor ring (branch-1). The RT producer holds `mon_tx` (pushes wet
    // mono); the cpal OUTPUT stream's callback holds `mon_rx` (behind an Arc<Mutex> so successive
    // arm/disarm streams reuse the one SPSC Consumer). `monitor_gain` (linear, default unity) is
    // passed in from `load` (the SlotHandle holds the same Arc for `set_monitor_gain`) and read
    // each callback; `monitor_starves` counts underrunning callbacks, mirrored into diag at the
    // gate emit.
    let (mon_tx, mon_rx) = RingBuffer::<f32>::new(OUT_RING_CAP);
    let rt_cfg = RtConfig {
        slot,
        shared_ptr,
        cap_frames,
        out_channels,
        in_channels,
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
    // back (`service_restart`); `None` after a failed restart = a silent, still-unloadable slot.
    // The load is reported only after the producer spawned (`spawn_rt_then_ready`).
    let mut rt_guard = spawn_rt_then_ready(
        &ready_tx,
        (info, shared_buf),
        || {
            spawn_rt(
                &rt_cfg,
                stopped,
                rings,
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
        let _ = instance.try_deactivate();
        return;
    }
    diag.alive.store(true, Relaxed);
    let mut native_io = NativeIo::new(
        slot,
        "CLAP",
        window.clone(),
        diag.clone(),
        in_channels > 0,
        input_producer,
        mon_rx,
        monitor_gain,
    );

    // Owner loop, all ON CLACK'S MAIN THREAD (where the !Send instance lives):
    //   deliver coalesced plugin callback requests, then
    //   1. service P9.5 state save/load requests (CLAP state is a main-thread call), and
    //   2. report any RT fault bits outside the audio thread, and
    //   3. (DEV) emit one grep-able gate line every ~2s. GateState holds the owner-local
    //      window-delta baselines + drift EWMA + run clock (one per load → M4-clean).
    // The 20 ms poll bounds callback delivery while idle without making the RT callback send through
    // a locking/allocating channel. Command messages still wake recv immediately; diagnostics stay ~2s.
    #[cfg(debug_assertions)]
    let mut gate_state = GateState::new();
    let emit_period = Duration::from_millis(2000);
    let mut last_emit = Instant::now();
    let mut reported_rt_faults = 0u32;
    // P10.0 editor state. `host_hwnd` (main window, 0 = unknown) is the editor's owner/transient
    // parent; process-stable, so read once. `editor` tracks what's open: a plugin-owned floating
    // window, or our host-owned top-level window the plugin embeds into (the latter needs us to
    // pump Win32 messages while it's open).
    let host_hwnd: usize = window.hwnd().map(|h| h.0 as usize).unwrap_or(0);
    let mut editor = EditorSlot::Closed;
    while running.load(Relaxed) {
        deliver_plugin_callback(&mut instance);
        // The plugin changed parameter values/info itself (params.rescan) → the web UI re-lists.
        if take_params_rescan(&mut instance) {
            refresh_clap_param_ids(&mut instance, &param_ids);
            let _ = window.emit("plugin:params-changed", slot);
        }
        // The plugin asked for a deactivate → activate cycle (host.request_restart).
        if instance.access_shared_handler(|s| s.restart_requested.swap(false, Acquire)) {
            match service_restart(&mut instance, &mut rt_guard, &rt_cfg, &diag) {
                Ok(()) => log::info!(
                    "[plugin_host] slot {slot} restarted at the plugin's request (deactivate → activate → RT resumed)"
                ),
                Err(e) => log::error!("[plugin_host] slot {slot} plugin-requested restart failed: {e}"),
            }
        }
        // (a) Floating editor: the plugin signalled `closed` (HostGuiImpl::closed) from its own
        //     window thread → ack on this (main) thread.
        if editor_closed.fired.swap(false, Acquire) && matches!(editor, EditorSlot::Floating) {
            let was_destroyed = editor_closed.was_destroyed.load(Relaxed);
            // Destroy the plugin GUI UNCONDITIONALLY before dropping to Closed. was_destroyed=true
            // REQUIRES the gui.destroy ack (CLAP contract); was_destroyed=false leaves a still-live
            // GUI — and since editor_open always gui.create()s (there is no "created-but-hidden"
            // re-show state), abandoning it in Closed makes the NEXT open a double gui.create, a
            // CLAP-contract violation that crashes JUCE. editor_destroy_gui = gui.hide+gui.destroy,
            // correct for both cases. (Bug-hunt 2026-06-21, #10.)
            editor_teardown(&mut instance, &mut editor, &hosted_hwnd);
            let _ = window.emit("plugin:editor-closed", slot);
            log::info!("[plugin_host] slot {slot} floating editor closed (was_destroyed={was_destroyed})");
        }
        // (b) Hosted editor: pump our window's messages (or the embedded plugin UI freezes), then
        //     react to the user clicking the close box (the wndproc set the flag).
        let hosted = matches!(editor, EditorSlot::Hosted(_));
        if hosted {
            pump_thread_messages();
            let user_closed = match &editor {
                EditorSlot::Hosted(hw) => hw.close_requested(),
                _ => false,
            };
            if user_closed {
                editor_teardown(&mut instance, &mut editor, &hosted_hwnd);
                let _ = window.emit("plugin:editor-closed", slot);
                log::info!("[plugin_host] slot {slot} hosted editor closed by user");
            }
        }
        // (c) Service one request: hosted → poll briefly (keep the UI responsive); else block until
        //     the next request or the gate deadline.
        let req = if matches!(editor, EditorSlot::Hosted(_)) {
            wait_for_input(20);
            request_rx.try_recv().ok()
        } else {
            let timeout = emit_period.saturating_sub(last_emit.elapsed()).min(CALLBACK_POLL_INTERVAL);
            match request_rx.recv_timeout(timeout) {
                Ok(r) => Some(r),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    // Senders dropped (teardown sets running=false first). Park briefly so we
                    // don't busy-spin before the `running` check exits the loop.
                    std::thread::sleep(Duration::from_millis(10));
                    None
                }
            }
        };
        if let Some(req) = req.and_then(|r| take_uncancelled(r, slot)) {
            match req {
                OwnerRequest::OpenEditor(cancelled, reply) => {
                    let was_closed = matches!(editor, EditorSlot::Closed);
                    let res = if was_closed {
                        match editor_open(&mut instance, host_hwnd, &hosted_hwnd) {
                            Ok(new_ed) => {
                                editor = new_ed;
                                Ok(())
                            }
                            Err(e) => Err(e),
                        }
                    } else {
                        Ok(()) // already open — idempotent
                    };
                    // A GUI that took >5s to come up is a LATE success: the caller reported failure
                    // and JS shows no editor, so destroy it again through the CloseEditor path. Only
                    // what THIS request opened — an editor that was already open belongs to an
                    // earlier successful open and stays up.
                    if was_closed && res.is_ok() && cancelled.load(Relaxed) {
                        log::warn!("[plugin_host] slot {slot} openEditor: owner-request cancelled mid-call (caller timed out) — rolling back, destroying the editor");
                        editor_teardown(&mut instance, &mut editor, &hosted_hwnd);
                    }
                    let _ = reply.send(res);
                }
                // No rollback: a late close lands where both sides converge. A CANCELLED close never
                // reaches here — it is skipped before it starts (`take_uncancelled`).
                OwnerRequest::CloseEditor(_, reply) => {
                    if !matches!(editor, EditorSlot::Closed) {
                        editor_teardown(&mut instance, &mut editor, &hosted_hwnd);
                    }
                    let _ = reply.send(Ok(()));
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
                other => handle_owner_request(other, &mut instance, &param_ids),
            }
        }
        native_io.poll_faults();
        if last_emit.elapsed() >= emit_period {
            native_io.mirror_diag();
            report_new_rt_faults(&diag, slot, &mut reported_rt_faults);
            #[cfg(debug_assertions)]
            emit_gate(&diag, &mut gate_state);
            last_emit = Instant::now();
        }
    }
    // P10.0: tear down a still-open editor before deactivating the instance (clean shutdown).
    if !matches!(editor, EditorSlot::Closed) {
        editor_teardown(&mut instance, &mut editor, &hosted_hwnd);
    }
    let _ = editor;
    // Stop native capture + monitor before joining the RT thread (which drops the ring ends).
    drop(native_io);

    // Shutdown handshake: stopping the guard makes the RT thread stop processing and return the
    // Send `Stopped` here so deactivate happens on the owner (clack main) thread.
    if let Some(guard) = rt_guard.take() {
        let rt_result = guard.stop_and_join();
        report_new_rt_faults(&diag, slot, &mut reported_rt_faults);
        match rt_result {
            Ok(exit) => instance.deactivate(exit.stopped),
            Err(_) => log::error!("[plugin_host] RT thread panicked; not cleanly deactivated"),
        }
    }
    diag.alive.store(false, Relaxed);
    // instance + _entry drop here.
}

/// The device-less CLAP RT producer. Renders the plugin at D (`device_rate`), sums to mono, and
/// hands each D-rate block to the shared `Hop1Pipe` (resample D→C + ring write + pacing). Returns
/// the `Stopped` processor + rings (`RtExit`) when its run flag drops — at unload, or mid-load for a
/// plugin-requested restart. ZERO heap allocation in the per-block body after warmup.
#[allow(clippy::too_many_arguments)]
fn producer_loop(
    stopped: StoppedPluginAudioProcessor<LfHost>,
    shared_ptr: usize,
    cap_frames: u32,
    diag: Arc<ProducerDiag>,
    running: Arc<AtomicBool>,
    out_channels: u32,
    in_channels: u32, // P11.1: plugin audio-input port-0 channels; 0 = no input bus (keep empty())
    period_frames: u32,
    block_config_gen_at_load: u32,
    max_frames: u32,
    sample_rate: f64, // C — the Web Audio ctx rate (hop-1 output is at C)
    device_rate: f64, // D — the rate the plugin renders at (resampled to C)
    drift_ppm: f64,   // the predecessor's learned hop-1 drift (0.0 at load)
    rings: RtRings,
) -> RtExit {
    let RtRings {
        mut event_rx,
        mut in_rx,
        mut mon_tx,
    } = rings;
    let mmcss = promote_pro_audio();

    let chans = out_channels.max(1) as usize;
    // P11.3 live buffer: period_frames + block are mutable so a config-generation bump can re-pace the
    // loop at a new D-block without re-activating the plugin (buffers stay sized to cap_buf =
    // max_frames = MAX_SELECTABLE_BLOCK + 128, so any pick up to MAX always fits the `[..block]`).
    let mut period_frames = period_frames;
    let mut block = period_frames as usize; // plugin render block = D-frames per process() call
    let cap_buf = max_frames as usize;
    // Pre-allocate everything the hot loop touches (invariant #5).
    let mut out_bufs: Vec<Vec<f32>> = vec![vec![0.0f32; cap_buf]; chans];
    let mut mono: Vec<f32> = vec![0.0f32; cap_buf]; // resampler INPUT (D-frames), sized to max
    let mut out_ports = AudioPorts::with_capacity(chans, 1);

    // P11.1: input bus pre-allocation (only when the plugin has one). Mono capture is duplicated
    // across all input rows (guitar is mono; a stereo-in FX gets the same signal both sides).
    // `in_chans.max(1)` keeps a harmless 1-row scratch even at 0 so the Vec/ports are always valid;
    // the per-block code branches on `in_chans == 0` to keep the exact P9 `empty()` path for synths.
    let in_chans = in_channels as usize;
    let mut in_bufs: Vec<Vec<f32>> = vec![vec![0.0f32; cap_buf]; in_chans.max(1)];
    let mut in_ports = AudioPorts::with_capacity(in_chans.max(1), 1);

    // P11 input-SRC: the cpal→D input resampler, (re)built on each arm/disarm/device-swap
    // (signalled by `diag.input_gen`). `None` = disarmed ⇒ feed silence. The build allocates
    // (rubato buffers + scratch) — an accepted ONE-SHOT cost at a user arm (not steady-state;
    // absorbed by the ~30ms hop-2 output buffer), kept OUTSIDE the rt_alloc guard so the
    // steady-state `rt_allocs:0` invariant holds. `in_gen` mirrors the last gen seen.
    let mut in_pipe: Option<InPipe> = None;
    let mut in_gen: u32 = 0;
    // P11.3: the native monitor pipe (branch-1), (re)built on each monitor arm/disarm/device-swap
    // (`monitor_gen`). `None` = monitor disarmed ⇒ don't push (the cpal output stream is gone). The
    // build allocates → kept OUTSIDE the rt_alloc guard, like `in_pipe`.
    let mut out_pipe: Option<OutMonitorPipe> = None;
    let mut mon_gen: u32 = 0;
    // The setup snapshots the global generation BEFORE it reads CHOSEN_BLOCK_FRAMES. If the command
    // runs anywhere after that snapshot, including before this slot is parked, the first loop sees
    // the mismatch and reconciles to the latest chosen value.
    let mut block_config_gen_seen = block_config_gen_at_load;

    // P9.5: pre-grown event buffer reused every block. `clear()` keeps capacity and `push()` is
    // alloc-free while the buffer stays under it — and the drain below is capped at
    // MAX_EVENTS_PER_BLOCK so it never exceeds. This (the only alloc here) is pre-loop. Replaces
    // the P9.3 single held DEV note: notes + params now arrive live over the rtrb ring.
    let mut event_buf = EventBuffer::with_capacity(MAX_EVENTS_PER_BLOCK);

    // The shared downstream pipe (resample D→C under the drift controller + drop-on-full hop-1
    // ring write + absolute-deadline pacing) — one copy for CLAP + VST3 (P10.1). Built here on the
    // RT thread; never crosses threads. The resampler inside is the only per-producer allocation.
    let mut pipe =
        match Hop1Pipe::new(shared_ptr, cap_frames, sample_rate, device_rate, period_frames) {
            Ok(p) => p,
            Err(_) => {
                diag.latch_rt_fault(RtFault::Hop1Rebuild);
                return RtExit {
                    stopped,
                    rings: RtRings {
                        event_rx,
                        in_rx,
                        mon_tx,
                    },
                    period_frames,
                    block_config_gen: block_config_gen_seen,
                    drift_ppm,
                };
            }
        };
    pipe.resume_drift_ppm(drift_ppm);

    let mut started = match stopped.start_processing() {
        Ok(s) => s,
        Err(e) => {
            diag.latch_rt_fault(RtFault::ClapProcess);
            return RtExit {
                stopped: e.into_stopped_processor(),
                rings: RtRings {
                    event_rx,
                    in_rx,
                    mon_tx,
                },
                period_frames,
                block_config_gen: block_config_gen_seen,
                drift_ppm: pipe.drift_ppm(),
            };
        }
    };

    let mut steady: u64 = 0;
    let mut warmup: u32 = 8; // settle one-time lazy allocs before measuring rt_allocs
    // An armed ASIO input clocks the producer (`Hop1Pipe::pace_on_input`); set at each input rebuild.
    let mut clocked = false;
    while running.load(Relaxed) {
        // P11 input-SRC: (re)build the input resampler on each arm/disarm/device-swap (gen bump).
        // OUTSIDE the rt_alloc guard below (InPipe::new allocates) → steady state stays
        // rt_allocs:0; this fires only on a control event. Flush stale ring frames first (prev
        // device / pre-arm accumulation) so a (re)arm starts clean — consumer side, cpal is the
        // sole producer. Gating on input_gen (not the rate) means a same-rate device swap still
        // rebuilds + flushes. Acquire pairs the owner's rate + actual-backend publication.
        if in_chans > 0 {
            let gen_now = diag.input_gen.load(Acquire);
            if gen_now != in_gen {
                in_gen = gen_now;
                let rate_now = diag.input_rate.load(Relaxed);
                while in_rx.pop().is_ok() {}
                pipe.mark_step(); // the flush and the change of pacing clock jump production
                clocked = false;
                in_pipe = if rate_now == 0 {
                    None
                } else {
                    let is_asio = diag.input_is_asio.load(Relaxed);
                    clocked = is_asio && diag.input_wake.handle().is_some();
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
        // P11.3: (re)build the monitor pipe on a monitor_gen bump (arm/disarm/device-swap), same
        // discipline as in_pipe — OUTSIDE the rt_alloc guard. `monitor_rate==0` ⇒ disarmed ⇒ None.
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
        // P11.3 live buffer: re-pace to a new D-block on a global config-generation bump. Runs
        // AFTER the input/monitor rebuilds so an arm-and-resize in the same tick lands on the
        // final block.
        // Does NOT flush in_rx (a buffer change must not drop live captured audio — unlike a device
        // swap). All rebuilds happen here, OUTSIDE the rt_alloc guard (they realloc rubato buffers).
        {
            let bg_now = BLOCK_CONFIG_GEN.load(Acquire);
            if bg_now != block_config_gen_seen {
                block_config_gen_seen = bg_now;
                let chosen = CHOSEN_BLOCK_FRAMES.load(Relaxed);
                let new_period =
                    (if chosen != 0 { chosen } else { period_frames }).min(MAX_SELECTABLE_BLOCK);
                // Rebuild ONLY on an actual block change. Hop1 first (it carries the cross-process
                // ring cursor): commit the block
                // change only if it rebuilt cleanly, else keep the old block (no desync). in/out
                // pipes disarm on their (can't-happen) failure, mirroring the ::new None-on-error.
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
                Some(super::rt_alloc::guard())
            } else {
                None
            };

            // P9.5: drain the main→audio ring into this block's events (alloc-free; `clear` keeps
            // capacity, `push` stays under it because the drain is capped). Any surplus beyond the
            // cap is NOT lost — it stays in the ring for the next block (≈10ms later). All events
            // are stamped time 0, so the buffer is trivially time-sorted (no `sort` needed).
            event_buf.clear();
            let mut drained = 0usize;
            while drained < MAX_EVENTS_PER_BLOCK {
                match event_rx.pop() {
                    Ok(ev) => {
                        push_event(&mut event_buf, ev);
                        drained += 1;
                    }
                    Err(_) => break,
                }
            }
            // P11 input-SRC: produce this block's mono input row from the cpal ring through the
            // R_in→D resampler (`in_pipe`). Disarmed (no pipe) ⇒ feed silence; the ring isn't
            // drained then (the rate-change rebuild flushes it). Mono row 0 is duplicated to any
            // further input rows. Skipped entirely for a synth (in_chans == 0) so its path is
            // byte-identical to P9. Alloc-free (the resampler + scratch are pre-built in InPipe).
            if in_chans > 0 {
                match in_pipe.as_mut() {
                    Some(p) => p.fill_block(&mut in_rx, &mut in_bufs[0], block, warmup == 0, clocked, &diag),
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
            // A plugin may report silence by leaving its outputs untouched. Clear reused rows first so
            // silence and process errors cannot replay the previous block. No allocation on the RT path.
            for ch in out_bufs.iter_mut() {
                for sample in ch[..block].iter_mut() {
                    *sample = 0.0;
                }
            }

            let status = {
                // Mirror the output-buffer construction. `InputChannel::variable` (NOT `::constant`,
                // which sets the CLAP constant_mask and lets a plugin skip a "constant" channel —
                // wrong for a live, sample-varying guitar signal). Synth ⇒ unchanged empty() path.
                let input_audio = if in_chans == 0 {
                    InputAudioBuffers::empty()
                } else {
                    in_ports.with_input_buffers([AudioPortBuffer {
                        latency: 0,
                        channels: AudioPortBufferType::f32_input_only(
                            // `&mut` reslice, mirroring the output side (`&mut c[..block]`): clack's
                            // InputChannel wraps CLAP's non-const `float**`, so it takes &mut.
                            in_bufs[..in_chans].iter_mut().map(|c| InputChannel::variable(&mut c[..block])),
                        ),
                    }])
                };
                let input_events = InputEvents::from_buffer(&event_buf);
                let mut output_events = OutputEvents::void();
                let mut output_audio = out_ports.with_output_buffers([AudioPortBuffer {
                    latency: 0,
                    channels: AudioPortBufferType::f32_output_only(
                        out_bufs.iter_mut().map(|c| &mut c[..block]),
                    ),
                }]);
                started.process(
                    &input_audio,
                    &mut output_audio,
                    &input_events,
                    &mut output_events,
                    Some(steady),
                    None,
                )
            };
            if status.is_err() {
                diag.latch_rt_fault(RtFault::ClapProcess);
            }

            sum_to_mono(&out_bufs, &mut mono, block, chans); // mono[..block] = D-frame input
            #[cfg(debug_assertions)]
            crate::marker_probe::inject(Arc::as_ptr(&diag) as usize, &mut mono[..block], device_rate);
            // Resample D→C, drop-on-full hop-1 ring write + diag mirror (drift engaged post-warmup).
            pipe.publish(&mono[..block], warmup == 0, &diag);
            // P11.3 branch-1: also push the SAME wet mono to the native monitor (when armed).
            if let Some(mp) = out_pipe.as_mut() {
                // One ASIO driver carries capture and monitor: the producer's clock is the monitor's.
                let same_clock = clocked
                    && in_pipe.is_some()
                    && diag.input_is_asio.load(Relaxed)
                    && diag.monitor_is_asio.load(Relaxed);
                let out_block = match diag.monitor_out_block.load(Relaxed) as usize {
                    0 => block,
                    n => n,
                };
                mp.publish(&mono[..block], warmup == 0, same_clock, out_block, &mut mon_tx, &diag);
            }
            steady = steady.wrapping_add(block as u64); // advance by D-frames (plugin phase at D)
        }

        if warmup > 0 {
            warmup -= 1;
            if warmup == 0 {
                #[cfg(debug_assertions)]
                super::rt_alloc::RT_ALLOCS.store(0, Relaxed);
            }
        }
        // An armed ASIO input clocks the producer (`pace_on_input`); otherwise the absolute-deadline timer.
        match (in_pipe.as_ref(), diag.input_wake.handle()) {
            (Some(p), Some(wake)) if clocked => pipe.pace_on_input(wake, || in_rx.slots() >= p.need()),
            _ => pipe.pace(&diag),
        }
    }

    let stopped = started.stop_processing();
    if let Some(h) = mmcss {
        revert(h);
    }
    RtExit {
        stopped,
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

/// `plugin_load` body (Windows). Spawns the owner thread, waits for its setup result, and on
/// success parks the control handle (incl. the agile SharedBuffer ref) in `state.slots`.
pub fn load(
    state: &PluginHostState,
    window: &tauri::WebviewWindow,
    slot: u8,
    path: String,
    id: String,
    frontend_epoch: u32,
    load_token: u32,
) -> Result<PluginInfo, String> {
    let sr_bits = state.sample_rate.load(Relaxed);
    let sample_rate = if sr_bits != 0 {
        f64::from_bits(sr_bits)
    } else {
        48_000.0 // host_init not called yet — fine as a fallback; the real path sends ctx rate
    };

    let running = Arc::new(AtomicBool::new(true));
    let load_gen = reserve_load(state, slot, frontend_epoch, &running)?;
    let diag = Arc::new(ProducerDiag::new());
    // P11.3: native-monitor gain (linear, unity). Created here so the SlotHandle and the owner
    // thread share ONE Arc — `set_monitor_gain` then stores from the command thread with no hop.
    let monitor_gain = Arc::new(AtomicU32::new(1.0f32.to_bits()));
    // The known param ids, filled by the owner thread before it reports the load (`ParamIds`).
    let param_ids = ParamIds::default();
    let (ready_tx, ready_rx) = load_ready_channel::<LoadReady>();

    // P9.5 control plane: the main→audio event ring (Producer kept here for the SlotHandle,
    // Consumer handed to the RT producer) + the main→owner state-request channel.
    let (event_tx, event_rx) = RingBuffer::<PluginEvent>::new(EVENT_RING_CAP);
    let event_tx = Arc::new(std::sync::Mutex::new(event_tx));
    let (request_tx, request_rx) = std::sync::mpsc::channel::<OwnerRequest>();

    let owner_running = running.clone();
    let owner_diag = diag.clone();
    let owner_window = window.clone();
    let owner_monitor_gain = monitor_gain.clone();
    let owner_param_ids = param_ids.clone();
    let owner_join = match std::thread::Builder::new()
        .name(format!("lf-clap-owner-{slot}"))
        .spawn(move || {
            owner_main(
                owner_window,
                path,
                id,
                sample_rate,
                frontend_epoch,
                load_token,
                load_gen,
                owner_running,
                owner_diag,
                ready_tx,
                slot,
                event_rx,
                request_rx,
                owner_monitor_gain,
                owner_param_ids,
            )
        })
    {
        Ok(join) => join,
        Err(e) => {
            clear_load_reservation(state, slot, load_gen);
            return Err(format!("spawn owner thread: {e}"));
        }
    };

    match ready_rx.recv_timeout(Duration::from_secs(15)) {
        Ok(Ok((info, shared_buf))) => park_loaded(
            state,
            window,
            slot,
            load_gen,
            frontend_epoch,
            SlotHandle {
                info,
                running,
                diag,
                owner_join,
                shared_buf,
                event_tx,
                request_tx,
                monitor_gain,
                param_ids,
            },
        ),
        Ok(Err(e)) => {
            running.store(false, Relaxed);
            let _ = owner_join.join();
            clear_load_reservation(state, slot, load_gen);
            Err(e)
        }
        Err(e) => {
            // Setup hung; detach the orphan (DEV-only failure path). A late result is the owner's
            // to undo: its send fails once `ready_rx` drops here (`spawn_rt_then_ready`).
            running.store(false, Relaxed);
            clear_load_reservation(state, slot, load_gen);
            Err(format!("plugin setup timed out: {e}"))
        }
    }
}

/// `plugin_list_loaded` body (Windows). Snapshot every currently-loaded slot's descriptor
/// (frontend-reload wedge resync, 2026-07-06). A WebView reload resets the FRONTEND to synth
/// defaults while these native slots stay loaded; the frontend queries this at init to find the
/// strays and unload them (via the production `unload`) before a later load hits "already has a
/// plugin loaded". Cheap + owner-hop-free: locks `slots`, clones the parked `info`.
pub fn list_loaded(state: &PluginHostState) -> Result<Vec<PluginInfo>, String> {
    let slots = state.slots.lock().map_err(|_| "slots lock poisoned")?;
    Ok(slots
        .iter()
        .filter_map(SlotState::loaded)
        .map(|h| h.info.clone())
        .collect())
}

/// `plugin_unload` body (Windows). Signals stop and joins the owner thread (which tears the RT
/// thread down + deactivates the instance), then `Close()`s the WebView2 SharedBuffer on the UI
/// thread. JS has already released its view (plugin-bridge.teardown) before this is invoked.
pub fn unload(
    state: &PluginHostState,
    window: &tauri::WebviewWindow,
    slot: u8,
) -> Result<(), String> {
    let handle = {
        let mut slots = state.slots.lock().map_err(|_| "slots lock poisoned")?;
        let previous = std::mem::replace(&mut slots[slot as usize], SlotState::Empty);
        match previous {
            SlotState::Empty => None,
            SlotState::Loading { running, .. } => {
                running.store(false, Release);
                None
            }
            SlotState::Loaded(handle) => Some(handle),
        }
    };
    if let Some(h) = handle {
        h.teardown(window);
    }
    Ok(())
}

/// `plugin_load` body for VST3 (Windows). Mirrors `load`: spawns the VST3 owner thread, waits for
/// its setup result, and parks the (format-agnostic) `SlotHandle` in `state.slots`. The only
/// difference from the CLAP `load` is the owner entry point (`vst3_host::vst3_owner_main`) — the
/// control plane (event ring, request channel, shared-buffer handle, diag, gate) is identical.
pub fn vst3_load(
    state: &PluginHostState,
    window: &tauri::WebviewWindow,
    slot: u8,
    path: String,
    id: String,
    frontend_epoch: u32,
    load_token: u32,
) -> Result<PluginInfo, String> {
    let sr_bits = state.sample_rate.load(Relaxed);
    let sample_rate = if sr_bits != 0 { f64::from_bits(sr_bits) } else { 48_000.0 };

    let running = Arc::new(AtomicBool::new(true));
    let load_gen = reserve_load(state, slot, frontend_epoch, &running)?;
    let diag = Arc::new(ProducerDiag::new());
    // P11.3 Stage B: native-monitor gain (linear, unity). Created here so the SlotHandle and the
    // VST3 owner thread share ONE Arc — `set_monitor_gain` stores from the command thread with no
    // hop, and the owner hands a clone to the cpal-out callback (open_output_stream). Mirrors CLAP.
    let monitor_gain = Arc::new(AtomicU32::new(1.0f32.to_bits()));
    // The known param ids, filled by the owner thread before it reports the load (`ParamIds`).
    let param_ids = ParamIds::default();
    let (ready_tx, ready_rx) = load_ready_channel::<LoadReady>();
    let (event_tx, event_rx) = RingBuffer::<PluginEvent>::new(EVENT_RING_CAP);
    let event_tx = Arc::new(std::sync::Mutex::new(event_tx));
    let (request_tx, request_rx) = std::sync::mpsc::channel::<OwnerRequest>();
    // P11.0 cpal→RT audio-input ring (mirrors CLAP): Consumer → the RT producer (via the owner
    // thread), Producer owner-local behind Arc<Mutex> so arm/disarm/re-arm feed the one SPSC ring.
    // Built here in rt_host scope (RingBuffer isn't imported in the vst3_host child mod) and handed
    // into vst3_owner_main, like event_rx.
    let (in_tx, in_rx) = RingBuffer::<f32>::new(IN_RING_CAP);
    let input_producer = Arc::new(std::sync::Mutex::new(in_tx));
    // P11.3 Stage B: the RT→cpal-out monitor ring (mirrors CLAP owner_main). Built here in rt_host
    // scope (RingBuffer isn't imported in the vst3_host child mod) and handed in: `mon_tx`
    // (Producer) → the RT producer, `mon_rx` (Consumer) → owner-local (wrapped Arc<Mutex> for the
    // cpal-out callback). `monitor_gain` is cloned so the SlotHandle + owner thread share ONE Arc —
    // `set_monitor_gain` then stores from the command thread with no owner hop.
    let (mon_tx, mon_rx) = RingBuffer::<f32>::new(OUT_RING_CAP);
    let owner_monitor_gain = monitor_gain.clone();

    let owner_running = running.clone();
    let owner_diag = diag.clone();
    let owner_window = window.clone();
    // Clone the producer for the owner thread (editor performEdit → ring) BEFORE event_tx moves into
    // SlotHandle. The Arc<Mutex<>> serialises owner + command pushes into the single SPSC ring.
    let owner_event_tx = event_tx.clone();
    let owner_param_ids = param_ids.clone();
    let owner_join = match std::thread::Builder::new()
        .name(format!("lf-vst3-owner-{slot}"))
        .spawn(move || {
            vst3_host::vst3_owner_main(
                owner_window,
                path,
                id,
                sample_rate,
                frontend_epoch,
                load_token,
                load_gen,
                owner_running,
                owner_diag,
                ready_tx,
                slot,
                event_rx,
                request_rx,
                owner_event_tx,
                in_rx,
                input_producer,
                mon_tx,
                mon_rx,
                owner_monitor_gain,
                owner_param_ids,
            )
        })
    {
        Ok(join) => join,
        Err(e) => {
            clear_load_reservation(state, slot, load_gen);
            return Err(format!("spawn vst3 owner thread: {e}"));
        }
    };

    match ready_rx.recv_timeout(Duration::from_secs(15)) {
        Ok(Ok((info, shared_buf))) => park_loaded(
            state,
            window,
            slot,
            load_gen,
            frontend_epoch,
            SlotHandle {
                info,
                running,
                diag,
                owner_join,
                shared_buf,
                event_tx,
                request_tx,
                monitor_gain,
                param_ids,
            },
        ),
        Ok(Err(e)) => {
            running.store(false, Relaxed);
            let _ = owner_join.join();
            clear_load_reservation(state, slot, load_gen);
            Err(e)
        }
        Err(e) => {
            running.store(false, Relaxed);
            clear_load_reservation(state, slot, load_gen);
            Err(format!("vst3 setup timed out: {e}"))
        }
    }
}

/// P10.1 — native VST3 host (second format). Reuses the shared transport (`Hop1Pipe` + the WebView2
/// SharedBuffer ring + drift control + gate) and the `PluginEvent` ring; only the upstream render
/// (module load → IComponent/IAudioProcessor → per-block VST3 `process()`) is new. Lives in a child
/// module so its `vst3::Steinberg` imports don't collide with clap's clack imports (both define
/// `NoteOnEvent`/`Event`). **Scope: scan + load + process + NOTES — the P10.1 gate.** The
/// controller (params/UI), state save/load, the floating/embedded editor, and the
/// separated-component path are deferred to P10.2/P10.3 (the owner replies "unsupported" to those
/// control requests). VST3 "COM" is Steinberg's FUnknown ABI (no apartments) — `ComPtr<I>` is
/// `Send` for VST3 interfaces (com-scrape emits explicit `unsafe impl Send`), so the processor
/// handle crosses owner→RT directly, no wrapper (the roadmap's planned `unsafe impl Send` is moot).

#[path = "vst3.rs"]
mod vst3_host;
