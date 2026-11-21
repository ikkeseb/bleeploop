//! OWNS: the chain unit's owner thread (E2 P3b; threads and gotchas: `src-tauri/AGENTS.md`): the
//! spawn/load API (`spawn_unit`, `load_unit`), the caller handle (`UnitOwner`), and the shared
//! owner loop (`serve_unit`) every plugin format runs through. Each chain unit gets its own owner
//! thread (`lf-{format}-unit-{id}`) that loads the `!Send` plugin, hands its `Send` unit to the
//! engine through [`UnitHost`], pumps Win32 messages, serves requests, keeps and retires its tone,
//! and tears it down in order.
//!
//! Legacy slots keep `engine_slot::serve` untouched; chain units run here beside it. Never drops a
//! unit box that may still live in the engine (`abandon` + `std::mem::forget`).

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{
    AtomicBool, AtomicU32,
    Ordering::{Acquire, Release},
};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Instant;

use clack_host::entry::PluginEntry;
use lf_engine::{SeatRefusal, UnitId, UnitProcessor};
use rtrb::{Consumer, Producer, RingBuffer};

use super::super::editor_window::{pump_thread_messages, wait_for_input};
use super::super::state::{ParamDesc, ToneRestore};
use super::super::tone::{ToneBinding, ToneKeeper};
use super::engine_slot::{
    keep_tone, own, report_faults, Editor, EngineSlotEvent, EventSink, LoadClaim, Loaded, OwnedPlugin, Place,
    PluginFormat, Reported, Setup, ASK_TIMEOUT, LOAD_TIMEOUT, OWNER_POLL,
};
use super::{
    load_ready_channel, publish_param_ids, take_uncancelled, OwnerRequest, ParamEvent, ParamIds, EVENT_RING_CAP,
};
use crate::engine_io::unit_host::{Activated, Back, Op, OpState, SubmitError, Terms, UnitHost};

/// A chain unit's load place: its slot index and the engine terms read before loading.
struct ChainPlace {
    slot: usize,
    terms: Terms,
}

impl Place for ChainPlace {
    fn slot(&self) -> usize {
        self.slot
    }

    fn rate(&self) -> Option<u32> {
        (self.terms.rate != 0).then_some(self.terms.rate)
    }

    fn max_block(&self) -> usize {
        self.terms.max_block
    }
}

/// How a unit's initial install settled, returned by `spawn_unit` / `load_unit`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)] // P4 / round 3 read all variants and fields
pub(crate) enum Answer {
    /// The install applied in the engine.
    Seated { name: String, tone: Option<ToneRestore> },
    /// Held in the slot's port (or queued behind a held message) until a capture ends.
    Waiting { name: String, tone: Option<ToneRestore> },
    /// The owner claimed the load and submitted the unit, and the engine had not answered within
    /// the second wait; the owner serves on (caller-only: the owner never sends this).
    Unanswered,
}

/// What a chain-unit owner thread starts with.
pub(super) struct UnitCtx {
    pub(super) unit: UnitHost,
    pub(super) running: Arc<AtomicBool>,
    /// Claimed before the unit is submitted (`LoadClaim`).
    pub(super) claim: Arc<LoadClaim>,
    pub(super) requests: Receiver<OwnerRequest>,
    /// The unit's end of the parameter event ring.
    pub(super) params: Consumer<ParamEvent>,
    /// The ring's producer, for a VST3 editor's `performEdit` (the handle holds the same one).
    pub(super) params_tx: Arc<Mutex<Producer<ParamEvent>>>,
    pub(super) param_ids: ParamIds,
    pub(super) sink: EventSink,
    /// The main window's HWND (0 = none): a floating CLAP editor's transient parent.
    pub(super) editor_parent: usize,
    /// Rendezvous (`load_ready_channel`): carries the install's answer (`Seated`/`Waiting`) or its
    /// error before seating.
    pub(super) ready: SyncSender<Result<Answer, String>>,
    /// The plugin's tone: restored at load, saved on edits/requests/teardown, retired on removal.
    pub(super) tone: ToneKeeper,
    /// Set once the plugin loads (before `claim`), so `UnitOwner::name` works even on `Unanswered`.
    shared_name: Arc<OnceLock<String>>,
}

/// Where the owner's unit box stands relative to the engine.
enum Seat<U> {
    /// Held by the owner, activated at `terms`, waiting to submit (initial submit or `PortFull`).
    InHand { unit: Box<U>, terms: Terms },
    /// Submitted to the engine; `op` is the install port message being polled.
    InFlight { op: Op },
    /// Applied in the engine.
    Seated,
    /// Out of the engine and parked (refused reseat, or round 1's placeholder for Withdrawn/Evicted).
    Parked { unit: Box<U> },
    /// Transient placeholder while `submit_in_hand` holds the unit box across `submit`/`reactivate`.
    InTransit,
}

impl<U> Seat<U> {
    fn take_unit(&mut self) -> Option<Box<U>> {
        match std::mem::replace(self, Seat::InTransit) {
            Seat::InHand { unit, .. } | Seat::Parked { unit } => Some(unit),
            other => {
                *self = other;
                None
            }
        }
    }
}

/// Why the owner's serving loop ended.
enum Exit<U> {
    /// `running` cleared or the request channel disconnected.
    Stopped,
    /// `Back::Removed` (`retire = true`) or `Back::Superseded` (`retire = false`) arrived.
    Returned { unit: Box<U>, retire: bool },
    /// `Back::Dismissed` arrived (the player removed a parked seat).
    Dismissed,
    /// Refused or fatally failed before ever seating.
    FailedBeforeSeat { unit: Option<Box<U>>, why: String },
}

fn refusal_reason(why: SeatRefusal) -> String {
    match why {
        SeatRefusal::Full => "the chain already seats the maximum number of units (Full)".to_string(),
        SeatRefusal::Latency => "the chain latency exceeds the maximum allowed (Latency)".to_string(),
        SeatRefusal::Id => "the unit id is invalid or already seated (Id)".to_string(),
        SeatRefusal::Placement => "the unit cannot be placed at that position in the chain (Placement)".to_string(),
        SeatRefusal::Unknown => "the engine refused the unit (Unknown)".to_string(),
    }
}

fn submit_error_reason(err: SubmitError) -> String {
    match err {
        SubmitError::Stale => "engine terms changed before submit".to_string(),
        SubmitError::PortFull => "the slot's engine port is full".to_string(),
        SubmitError::Superseded => "the unit was superseded before seating".to_string(),
        SubmitError::Closing => "the audio host is closing".to_string(),
        SubmitError::NoEngine => "no audio device is open".to_string(),
    }
}

fn send_answer(ready: &mut Option<SyncSender<Result<Answer, String>>>, slot: usize, id: UnitId, answer: Answer) {
    if let Some(tx) = ready.take() {
        if tx.send(Ok(answer)).is_err() {
            log::info!("[plugin_host] engine slot {slot} unit {id}: install answered after caller moved on; serving on");
        }
    }
}

fn drain_restart<P: OwnedPlugin>(plugin: &mut P, slot: usize, id: UnitId) {
    if let Some(why) = plugin.take_restart() {
        log::info!(
            "[plugin_host] engine slot {slot} unit {id}: ignoring restart request ({why}); reseat not built yet"
        );
    }
}

/// Submit the unit held in `Seat::InHand`, retrying on `SubmitError::Stale` by reactivating at the
/// fresh `host.terms()`. `PortFull` leaves the unit in hand for the next turn.
fn submit_in_hand<P: OwnedPlugin>(
    host: &UnitHost,
    plugin: &mut P,
    name: &str,
    seat: &mut Seat<P::Unit>,
    ever_seated: bool,
) -> Option<Exit<P::Unit>> {
    let (mut unit, mut terms) = match std::mem::replace(seat, Seat::InTransit) {
        Seat::InHand { unit, terms } => (unit, terms),
        other => {
            *seat = other;
            return None;
        }
    };
    const MAX_STALE_RETRIES: usize = 8;
    for attempt in 0..MAX_STALE_RETRIES {
        match host.submit(unit, terms) {
            Ok(op) => {
                *seat = Seat::InFlight { op };
                return None;
            }
            Err((returned, SubmitError::PortFull)) => {
                *seat = Seat::InHand { unit: own(returned), terms };
                return None;
            }
            Err((returned, SubmitError::Stale)) => {
                let returned = own(returned);
                let Some(fresh) = host.terms() else {
                    let why = "no audio device is open".to_string();
                    if !ever_seated {
                        return Some(Exit::FailedBeforeSeat { unit: Some(returned), why });
                    }
                    host.parked(why);
                    *seat = Seat::Parked { unit: returned };
                    return None;
                };
                if attempt + 1 == MAX_STALE_RETRIES {
                    *seat = Seat::InHand { unit: returned, terms };
                    return None;
                }
                match plugin.reactivate(returned, fresh) {
                    Ok(rearmed) => {
                        let info = rearmed.info();
                        host.activated(Activated {
                            name: name.to_string(),
                            source: info.source,
                            latency: info.latency,
                        });
                        unit = rearmed;
                        terms = fresh;
                    }
                    Err((returned, why)) => {
                        if !ever_seated {
                            return Some(Exit::FailedBeforeSeat { unit: Some(returned), why });
                        }
                        host.parked(why);
                        *seat = Seat::Parked { unit: returned };
                        return None;
                    }
                }
            }
            Err((returned, err)) => {
                let returned = own(returned);
                let why = submit_error_reason(err);
                if !ever_seated {
                    return Some(Exit::FailedBeforeSeat { unit: Some(returned), why });
                }
                log::warn!(
                    "[plugin_host] engine slot {} unit {}: submit failed ({why}); parking",
                    host.slot(),
                    host.id()
                );
                host.parked(why);
                *seat = Seat::Parked { unit: returned };
                return None;
            }
        }
    }
    unreachable!()
}

/// End before the unit ever applied a seat: fail the plate, release the plugin and its unit (if
/// held), release the reservation, and then answer `Err(why)` so a caller that sees `Err` finds the
/// plugin already gone.
fn fail_before_seat<P: OwnedPlugin>(
    host: UnitHost,
    mut plugin: P,
    unit: Option<Box<P::Unit>>,
    why: String,
    ready: &mut Option<SyncSender<Result<Answer, String>>>,
    faults: &AtomicU32,
    reported: &mut u32,
) -> Result<(), String> {
    let slot = host.slot();
    host.failed(why.clone());
    if plugin.editor() != Editor::Closed {
        plugin.editor_close();
    }
    plugin.release(unit);
    report_faults(faults, slot, reported);
    host.release();
    if let Some(tx) = ready.take() {
        let _ = tx.send(Err(why));
    }
    Ok(())
}

/// Ordered release once the unit is out of the engine (or none is held after `Dismissed`): close any
/// editor → fold a last `turn` into the tone → `quiesce` → save if dirty or editor was open →
/// optionally `retire` → `release(unit)` → `host.release()`. Skips `turn`, save and `retire` after a
/// panic.
#[allow(clippy::too_many_arguments)]
fn ordered_release<P: OwnedPlugin>(
    host: UnitHost,
    mut plugin: P,
    unit: Option<Box<P::Unit>>,
    tone: &mut ToneKeeper,
    name: &str,
    allow_save: bool,
    retire: bool,
    faults: &AtomicU32,
    reported: &mut u32,
) -> Result<(), String> {
    let slot = host.slot();
    let id = host.id();
    let t = Instant::now();
    let editor_open = plugin.editor() != Editor::Closed;
    if editor_open {
        plugin.editor_close();
    }
    if allow_save {
        let Reported { relist, changed } = plugin.turn(&(Arc::new(|_| {}) as EventSink));
        if relist || changed {
            tone.note_change(Instant::now());
        }
    }
    let keep = allow_save && (editor_open || tone.dirty());
    plugin.quiesce();
    if keep {
        let _ = keep_tone(tone, slot, "unload", name, || plugin.state());
    }
    if allow_save && retire {
        tone.retire();
    }
    plugin.release(unit);
    report_faults(faults, slot, reported);
    host.release();
    log::info!(
        "[plugin_host] engine slot {slot} unit {id} {} teardown: release={} ms",
        P::FORMAT,
        t.elapsed().as_millis()
    );
    Ok(())
}

/// The unit is still inside the engine when the loop ended (stop or panic): save if allowed and
/// dirty, abandon the row so any late box leaks, leak the plugin rather than unloading code the
/// audio thread may still execute, and release the reservation.
#[allow(clippy::too_many_arguments)]
fn abandon_in_engine<P: OwnedPlugin>(
    host: UnitHost,
    mut plugin: P,
    tone: &mut ToneKeeper,
    name: &str,
    allow_save: bool,
    faults: &AtomicU32,
    reported: &mut u32,
) -> Result<(), String> {
    let slot = host.slot();
    let id = host.id();
    let editor_open = plugin.editor() != Editor::Closed;
    if editor_open {
        plugin.editor_close();
    }
    if allow_save {
        let Reported { relist, changed } = plugin.turn(&(Arc::new(|_| {}) as EventSink));
        if relist || changed {
            tone.note_change(Instant::now());
        }
        if editor_open || tone.dirty() {
            let _ = keep_tone(tone, slot, "unload", name, || plugin.state());
        }
    }
    let msg = format!(
        "engine slot {slot} unit {id} {} teardown: unit is still in the engine; leaving the plugin loaded",
        P::FORMAT
    );
    log::error!("[plugin_host] {msg}");
    host.abandon();
    std::mem::forget(plugin);
    report_faults(faults, slot, reported);
    host.release();
    Err(msg)
}

/// The chain-unit owner loop every format shares: `load` builds the plugin and its unit from
/// `Setup`, this publishes params and plate metadata, claims, submits through [`UnitHost`], answers
/// the caller once (`Seated` or `Waiting`), serves requests and mailboxes, and tears down in order.
pub(super) fn serve_unit<P: OwnedPlugin>(
    ctx: UnitCtx,
    load: impl FnOnce(Setup<'_>) -> Result<Loaded<P>, String>,
) -> Result<(), String> {
    let UnitCtx {
        unit: host,
        running,
        claim,
        requests,
        params,
        params_tx,
        param_ids,
        sink,
        editor_parent,
        ready,
        mut tone,
        shared_name,
    } = ctx;
    let slot = host.slot();
    let id = host.id();
    let faults = Arc::new(AtomicU32::new(0));
    let mut reported = 0u32;
    let mut ready = Some(ready);

    let Some(terms) = host.terms() else {
        let why = "no audio device is open".to_string();
        host.failed(why.clone());
        host.release();
        if let Some(tx) = ready.take() {
            let _ = tx.send(Err(why));
        }
        return Ok(());
    };

    let place = ChainPlace { slot, terms };
    let setup = Setup {
        slot: &place,
        params,
        params_tx,
        sink: &sink,
        editor_parent,
        faults: faults.clone(),
        tone: &mut tone,
    };
    let Loaded { mut plugin, unit, name, tone: restored, .. } = match load(setup) {
        Ok(loaded) => loaded,
        Err(e) => {
            host.failed(e.clone());
            host.release();
            if let Some(tx) = ready.take() {
                let _ = tx.send(Err(e));
            }
            return Ok(());
        }
    };

    publish_param_ids(&param_ids, &plugin.params().unwrap_or_default());
    let _ = shared_name.set(name.clone());
    let info = unit.info();
    host.activated(Activated { name: name.clone(), source: info.source, latency: info.latency });

    if !claim.claim() {
        return fail_before_seat(
            host,
            plugin,
            Some(unit),
            "plugin load cancelled".to_string(),
            &mut ready,
            &faults,
            &mut reported,
        );
    }

    let mut seat = Seat::InHand { unit, terms };
    let mut ever_seated = false;
    let save = |tone: &mut ToneKeeper, plugin: &mut P, why: &str| keep_tone(tone, slot, why, &name, || plugin.state());

    // A panic in the loop must not unwind past a unit the engine still runs (dropping the plugin
    // would deactivate and unload code the audio thread is inside): catch it here and check whether
    // the unit is back in hand or still in the engine.
    let served = catch_unwind(AssertUnwindSafe(|| {
        while running.load(Acquire) {
            while let Some(back) = host.take_back() {
                match back {
                    Back::Removed(unit) => {
                        return Exit::Returned { unit: own(unit), retire: true };
                    }
                    Back::Superseded(unit) => {
                        return Exit::Returned { unit: own(unit), retire: false };
                    }
                    Back::Dismissed => {
                        return Exit::Dismissed;
                    }
                    Back::Refused { unit, why } => {
                        let unit = own(unit);
                        let msg = refusal_reason(why);
                        if !ever_seated {
                            return Exit::FailedBeforeSeat { unit: Some(unit), why: msg };
                        }
                        log::warn!("[plugin_host] engine slot {slot} unit {id}: reseat refused ({msg}); parking");
                        host.parked(msg);
                        seat = Seat::Parked { unit };
                    }
                    Back::Withdrawn { unit, .. } => {
                        log::warn!(
                            "[plugin_host] engine slot {slot} unit {id}: withdrawn; reseat not built yet, parking"
                        );
                        host.parked("reseat not built yet".to_string());
                        seat = Seat::Parked { unit: own(unit) };
                    }
                    Back::Evicted { unit, .. } => {
                        log::warn!(
                            "[plugin_host] engine slot {slot} unit {id}: evicted; reseat not built yet, parking"
                        );
                        host.parked("reseat not built yet".to_string());
                        seat = Seat::Parked { unit: own(unit) };
                    }
                }
            }

            if matches!(seat, Seat::InHand { .. }) {
                if let Some(exit) = submit_in_hand(&host, &mut plugin, &name, &mut seat, ever_seated) {
                    return exit;
                }
            }

            if let Seat::InFlight { op } = seat {
                match host.poll(op) {
                    OpState::Applied => {
                        seat = Seat::Seated;
                        ever_seated = true;
                        send_answer(&mut ready, slot, id, Answer::Seated { name: name.clone(), tone: restored });
                    }
                    OpState::Held => {
                        send_answer(&mut ready, slot, id, Answer::Waiting { name: name.clone(), tone: restored });
                    }
                    OpState::Pending if host.held() => {
                        send_answer(&mut ready, slot, id, Answer::Waiting { name: name.clone(), tone: restored });
                    }
                    OpState::Pending | OpState::Refused(_) | OpState::Rebuilt => {}
                }
            }

            drain_restart(&mut plugin, slot, id);
            let Reported { relist, changed } = plugin.turn(&sink);
            if relist {
                publish_param_ids(&param_ids, &plugin.params().unwrap_or_default());
                sink(EngineSlotEvent::ParamsChanged);
            }
            if relist || changed {
                tone.note_change(Instant::now());
            }

            // Every turn, editor or not (JUCE message thread + hosted editor close box).
            pump_thread_messages();
            let closed = plugin.idle();
            drain_restart(&mut plugin, slot, id);
            if closed {
                sink(EngineSlotEvent::EditorClosed);
                let _ = save(&mut tone, &mut plugin, "editor closed");
            }

            let request = if plugin.editor() == Editor::Hosted {
                wait_for_input(20);
                requests.try_recv().ok()
            } else {
                match requests.recv_timeout(OWNER_POLL) {
                    Ok(request) => Some(request),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            };
            if request.is_some() {
                drain_restart(&mut plugin, slot, id);
            }
            if let Some(request) = request.and_then(|r| take_uncancelled(r, slot as u8)) {
                match request {
                    OwnerRequest::OpenEditor(_, reply) => {
                        let was_closed = plugin.editor() == Editor::Closed;
                        let res = if was_closed { plugin.editor_open() } else { Ok(()) };
                        let opened = was_closed && res.is_ok();
                        if reply.send(res).is_err() && opened {
                            plugin.editor_close();
                        }
                    }
                    OwnerRequest::CloseEditor(_, reply) => {
                        if plugin.editor() != Editor::Closed {
                            plugin.editor_close();
                            let _ = save(&mut tone, &mut plugin, "editor closed");
                        }
                        let _ = reply.send(Ok(()));
                    }
                    OwnerRequest::SaveTone(reply) => {
                        let _ = reply.send(save(&mut tone, &mut plugin, "asked"));
                    }
                    OwnerRequest::ListParams(reply) => {
                        let res = plugin.params();
                        if let Ok(listed) = &res {
                            publish_param_ids(&param_ids, listed);
                        }
                        let _ = reply.send(res);
                    }
                    OwnerRequest::SetParamNormalized(id, value) => plugin.set_param_normalized(id, value),
                    OwnerRequest::Wake => {}
                }
            }

            if tone.poll(Instant::now()) {
                let _ = save(&mut tone, &mut plugin, "changes went quiet");
            }
            report_faults(&faults, slot, &mut reported);
        }
        Exit::Stopped
    }));

    match served {
        Ok(Exit::FailedBeforeSeat { unit, why }) => {
            fail_before_seat(host, plugin, unit, why, &mut ready, &faults, &mut reported)
        }
        Ok(Exit::Returned { unit, retire }) => {
            let res = ordered_release(host, plugin, Some(unit), &mut tone, &name, true, retire, &faults, &mut reported);
            if let Some(tx) = ready.take() {
                let _ = tx.send(Err("unit left before seating".to_string()));
            }
            res
        }
        Ok(Exit::Dismissed) => {
            let unit = seat.take_unit();
            let res = ordered_release(host, plugin, unit, &mut tone, &name, true, true, &faults, &mut reported);
            if let Some(tx) = ready.take() {
                let _ = tx.send(Err("unit dismissed before seating".to_string()));
            }
            res
        }
        Ok(Exit::Stopped) | Err(_) => {
            let panicked = served.is_err();
            if panicked {
                log::error!(
                    "[plugin_host] engine slot {slot} unit {id}: the {} owner panicked; tearing the plugin down",
                    P::FORMAT
                );
            }
            let mut in_hand = seat.take_unit();
            let mut retire = false;
            while let Some(back) = host.take_back() {
                match back {
                    Back::Removed(u) => {
                        in_hand = Some(own(u));
                        retire = true;
                    }
                    Back::Superseded(u) => {
                        in_hand = Some(own(u));
                        retire = false;
                    }
                    Back::Withdrawn { unit: u, .. } | Back::Evicted { unit: u, .. } | Back::Refused { unit: u, .. } => {
                        in_hand = Some(own(u));
                    }
                    Back::Dismissed => {
                        retire = true;
                    }
                }
            }
            if let Some(unit) = in_hand {
                ordered_release(
                    host,
                    plugin,
                    Some(unit),
                    &mut tone,
                    &name,
                    !panicked,
                    !panicked && retire,
                    &faults,
                    &mut reported,
                )
            } else {
                abandon_in_engine(host, plugin, &mut tone, &name, !panicked, &faults, &mut reported)
            }
        }
    }
}

/// Load plugin `id` from `path` into a chain unit owned through `unit`.
#[allow(dead_code)] // P4 calls this
pub(crate) fn load_unit(
    format: PluginFormat,
    path: String,
    id: String,
    unit: UnitHost,
    editor_parent: usize,
    sink: EventSink,
    tone: Option<ToneBinding>,
) -> Result<(UnitOwner, Answer), String> {
    match format {
        PluginFormat::Clap => spawn_unit(format, unit, editor_parent, sink, tone, move |ctx| {
            super::clap_engine::run_unit(ctx, &id, move || {
                // SAFETY: PluginEntry::load runs the bundle's foreign entry-init code; the
                // out-of-process scan vetted the bundle.
                unsafe { PluginEntry::load(&path) }.map_err(|e| format!("load failed: {e}"))
            })
        }),
        PluginFormat::Vst3 => spawn_unit(format, unit, editor_parent, sink, tone, move |ctx| {
            super::vst3_host::engine::run_unit(ctx, path, &id)
        }),
        PluginFormat::Vst2 => spawn_unit(format, unit, editor_parent, sink, tone, move |ctx| {
            super::vst2_engine::run_unit(ctx, path, &id)
        }),
    }
}

/// Spawn a chain-unit owner thread (`lf-{format}-unit-{id}`) running `run` and wait for the initial
/// install's answer (`Seated`/`Waiting`, or `Unanswered` if the claim stood past a second wait).
pub(super) fn spawn_unit(
    format: PluginFormat,
    unit: UnitHost,
    editor_parent: usize,
    sink: EventSink,
    tone: Option<ToneBinding>,
    run: impl FnOnce(UnitCtx) -> Result<(), String> + Send + 'static,
) -> Result<(UnitOwner, Answer), String> {
    let slot = unit.slot();
    let id = unit.id();
    let running = Arc::new(AtomicBool::new(true));
    let claim = Arc::new(LoadClaim::default());
    let edited = Arc::new(AtomicBool::new(false));
    let (params_tx, params) = RingBuffer::<ParamEvent>::new(EVENT_RING_CAP);
    let params_tx = Arc::new(Mutex::new(params_tx));
    let (request_tx, requests) = std::sync::mpsc::channel();
    let (ready, ready_rx) = load_ready_channel();
    let param_ids = ParamIds::default();
    let shared_name = Arc::new(OnceLock::new());
    let ctx = UnitCtx {
        unit,
        running: running.clone(),
        claim: claim.clone(),
        requests,
        params,
        params_tx: params_tx.clone(),
        param_ids: param_ids.clone(),
        sink,
        editor_parent,
        ready,
        tone: ToneKeeper::new(tone, edited.clone()),
        shared_name: shared_name.clone(),
    };
    let owner = std::thread::Builder::new()
        .name(format!("lf-{}-unit-{id}", format.as_str()))
        .spawn(move || run(ctx))
        .map_err(|e| format!("spawn unit owner thread: {e}"))?;

    let build_handle = |name: String, owner: JoinHandle<Result<(), String>>| UnitOwner {
        slot,
        id,
        format,
        name,
        running: running.clone(),
        owner: Some(owner),
        requests: request_tx,
        params_tx,
        param_ids,
        edited,
    };

    let report = match ready_rx.recv_timeout(LOAD_TIMEOUT) {
        Err(RecvTimeoutError::Timeout) => {
            if claim.abandon() {
                // Setup hung before claiming: the owner will release at its claim check.
                running.store(false, Release);
                return Err("plugin setup timed out".to_string());
            }
            log::warn!(
                "[plugin_host] engine slot {slot} unit {id}: setup outlasted {} ms after claim; waiting for engine answer",
                LOAD_TIMEOUT.as_millis()
            );
            match ready_rx.recv_timeout(LOAD_TIMEOUT) {
                Ok(ans) => Ok(ans),
                Err(RecvTimeoutError::Timeout) => {
                    drop(ready_rx);
                    let name = shared_name.get().cloned().unwrap_or_default();
                    return Ok((build_handle(name, owner), Answer::Unanswered));
                }
                Err(RecvTimeoutError::Disconnected) => Err(RecvTimeoutError::Disconnected),
            }
        }
        other => other,
    };

    match report {
        Ok(Ok(answer)) => {
            let name = match &answer {
                Answer::Seated { name, .. } | Answer::Waiting { name, .. } => name.clone(),
                Answer::Unanswered => shared_name.get().cloned().unwrap_or_default(),
            };
            Ok((build_handle(name, owner), answer))
        }
        Ok(Err(e)) => {
            running.store(false, Release);
            let _ = owner.join();
            Err(e)
        }
        Err(_) => {
            running.store(false, Release);
            let _ = owner.join();
            Err("plugin unit owner exited before answering".to_string())
        }
    }
}

/// Caller handle for a running chain-unit owner thread. Dropping it stops and joins the owner.
pub(crate) struct UnitOwner {
    slot: usize,
    id: UnitId,
    #[allow(dead_code)] // round 3: set_param
    format: PluginFormat,
    name: String,
    running: Arc<AtomicBool>,
    owner: Option<JoinHandle<Result<(), String>>>,
    requests: Sender<OwnerRequest>,
    #[allow(dead_code)] // round 3: set_param
    params_tx: Arc<Mutex<Producer<ParamEvent>>>,
    #[allow(dead_code)] // round 3: set_param
    param_ids: ParamIds,
    #[allow(dead_code)] // round 3: set_param
    edited: Arc<AtomicBool>,
}

#[allow(dead_code)] // P4 / tests call these methods
impl UnitOwner {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// The plugin's parameters, with live values; refreshes the ids `set_param` accepts.
    pub(crate) fn list_params(&self) -> Result<Vec<ParamDesc>, String> {
        self.ask("list_params", OwnerRequest::ListParams)
    }

    /// Save the plugin's tone now, through its owner, into the store; returns the tone file's bytes.
    pub(crate) fn take_tone(&self) -> Result<Vec<u8>, String> {
        self.ask("take_tone", OwnerRequest::SaveTone)
    }

    /// Whether the owner thread has exited (e.g. after `Back::Removed` tore it down).
    pub(crate) fn is_finished(&self) -> bool {
        self.owner.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Ask the owner loop to exit, wake it, and join the thread.
    pub(crate) fn stop(mut self) -> Result<(), String> {
        self.shutdown()
    }

    fn shutdown(&mut self) -> Result<(), String> {
        let Some(owner) = self.owner.take() else {
            return Ok(());
        };
        let t = Instant::now();
        self.running.store(false, Release);
        let _ = self.requests.send(OwnerRequest::Wake);
        let result = owner.join().unwrap_or_else(|_| Err("the unit owner thread panicked".to_string()));
        log::info!(
            "[plugin_host] engine slot {} unit {} owner joined in {} ms",
            self.slot,
            self.id,
            t.elapsed().as_millis()
        );
        result
    }

    /// One owner round trip for a request with a typed reply, bounded by [`ASK_TIMEOUT`].
    fn ask<T>(
        &self,
        op: &str,
        build: impl FnOnce(SyncSender<Result<T, String>>) -> OwnerRequest,
    ) -> Result<T, String> {
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        self.requests
            .send(build(reply_tx))
            .map_err(|_| "unit owner thread gone".to_string())?;
        reply_rx
            .recv_timeout(ASK_TIMEOUT)
            .map_err(|e| format!("{op} timed out: {e}"))?
    }
}

impl Drop for UnitOwner {
    fn drop(&mut self) {
        if let Err(e) = self.shutdown() {
            log::error!("[plugin_host] engine slot {} unit {} stop: {e}", self.slot, self.id);
        }
    }
}
