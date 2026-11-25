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
use std::time::{Duration, Instant};

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
    owner_request_5s, publish_param_ids, take_uncancelled, OwnerRequest, ParamEvent, ParamIds, EVENT_RING_CAP,
};
use crate::engine_io::unit_host::{Activated, Back, Op, OpState, SubmitError, Terms, UnitHost};

/// Bound for a stopping owner to collect a unit box already on its way back from the engine before
/// declaring it abandoned (matches production `REMOVE_TIMEOUT`).
const STOP_RETURN_TIMEOUT: Duration = Duration::from_secs(2);

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
    /// Capacity-1 reply channel (`ready_channel`): carries the install's first answer
    /// (`Seated`/`Waiting`) or its error before seating without blocking the owner.
    pub(super) ready: SyncSender<Result<Answer, String>>,
    /// The plugin's tone: restored at load, saved on edits/requests/teardown, retired on removal.
    pub(super) tone: ToneKeeper,
    /// Set once the plugin loads (before `claim`), so `UnitOwner::name` works even on `Unanswered`.
    pub(super) shared_name: Arc<OnceLock<String>>,
}

/// Capacity-1 first-answer channel so the owner's `try_send` never blocks waiting on its caller.
pub(super) fn ready_channel() -> (SyncSender<Result<Answer, String>>, Receiver<Result<Answer, String>>) {
    std::sync::mpsc::sync_channel(1)
}

/// Where the owner's unit box stands relative to the engine.
enum Seat<U> {
    /// Held by the owner and owed a seat (initial load, `Withdrawn`, `Evicted`, `Stale`/`PortFull`/
    /// `NoEngine`, or a parked unit woken by a restart). `activated_at` is `Some(terms)` only when
    /// the unit is already activated at those exact terms and has not returned from the engine or
    /// been asked to restart since.
    Owed { unit: Box<U>, activated_at: Option<Terms> },
    /// Submitted to the engine; `op` is the install port message being polled.
    Installing { op: Op },
    /// Applied in the engine.
    Seated,
    /// Withdrawal sent (`UnitHost::withdraw`); `op` is polled each turn so an idle engine still
    /// services it, while the unit box stays in the engine until `Back::Withdrawn` (or
    /// `Evicted`/`Removed`/`Superseded`) arrives.
    Withdrawing { op: Op },
    /// Out of the engine and parked (refused reseat or failed reactivate): the seat stays,
    /// `host.parked(why)` was reported. Retries on the next plugin-asked restart; ends on
    /// `Back::Dismissed`.
    Parked { unit: Box<U> },
    /// Out of the engine while the host is closing (`SubmitError::Closing`): kept in hand with no
    /// retry until shutdown releases it.
    Closing { unit: Box<U> },
    /// Out of the engine while a removal is pending or settled (`SubmitError::Removing`): kept in
    /// hand with no further `submit` until `Back::Dismissed`/`Back::Removed` (or shutdown) arrives.
    Removing { unit: Box<U> },
    /// Transient placeholder while moving the unit box across `reactivate`/`submit`.
    InTransit,
}

impl<U> Seat<U> {
    fn take_unit(&mut self) -> Option<Box<U>> {
        match std::mem::replace(self, Seat::InTransit) {
            Seat::Owed { unit, .. } | Seat::Parked { unit } | Seat::Closing { unit } | Seat::Removing { unit } => {
                Some(unit)
            }
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
    /// `Back::Dismissed` arrived (the seat was removed while its box was out of the engine).
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
        SubmitError::Replaced => "a newer source replaced this unit".to_string(),
        SubmitError::Removing => "the unit is being removed".to_string(),
        SubmitError::Reloading => "the unit is reloading".to_string(),
        SubmitError::Closing => "the audio host is closing".to_string(),
        SubmitError::NoEngine => "no audio device is open".to_string(),
    }
}

fn send_answer(ready: &mut Option<SyncSender<Result<Answer, String>>>, slot: usize, id: UnitId, answer: Answer) {
    if let Some(tx) = ready.take() {
        if tx.try_send(Ok(answer)).is_err() {
            log::info!("[plugin_host] engine slot {slot} unit {id}: install answered after caller moved on; serving on");
        }
    }
}

/// Poll the plugin for a restart request and act on (or remember) it against `seat`.
fn note_restart<P: OwnedPlugin>(
    host: &UnitHost,
    plugin: &mut P,
    seat: &mut Seat<P::Unit>,
    pending_restart: &mut Option<String>,
) {
    if let Some(why) = plugin.take_restart() {
        log::info!(
            "[plugin_host] engine slot {} unit {}: plugin requested restart ({why})",
            host.slot(),
            host.id()
        );
        *pending_restart = Some(why);
    }
    apply_pending_restart(host, seat, pending_restart);
}

/// Act on a pending restart request against the current `seat`.
fn apply_pending_restart<U>(host: &UnitHost, seat: &mut Seat<U>, pending_restart: &mut Option<String>) {
    let Some(why) = pending_restart.as_deref() else {
        return;
    };
    let slot = host.slot();
    let id = host.id();
    match seat {
        Seat::Seated => match host.withdraw() {
            Ok(op) => {
                *seat = Seat::Withdrawing { op };
                *pending_restart = None;
            }
            Err(SubmitError::PortFull | SubmitError::Reloading) => {
                // Keep pending_restart set and retry `withdraw()` on the next turn.
            }
            Err(SubmitError::Stale) => {
                log::info!(
                    "[plugin_host] engine slot {slot} unit {id}: dropping restart ({why}); engine rebuilt (eviction will reactivate)"
                );
                *pending_restart = None;
            }
            Err(err @ (SubmitError::Superseded | SubmitError::Replaced | SubmitError::Removing | SubmitError::Closing | SubmitError::NoEngine)) => {
                log::info!(
                    "[plugin_host] engine slot {slot} unit {id}: dropping restart ({why}); withdraw refused ({err:?})"
                );
                *pending_restart = None;
            }
        },
        Seat::Installing { .. } => {
            // Op in flight: remember the restart and withdraw once seated.
        }
        Seat::Withdrawing { .. } => {
            // Already withdrawing: the upcoming `Back::Withdrawn` will reactivate with latest state.
            *pending_restart = None;
        }
        Seat::Owed { activated_at, .. } => {
            *activated_at = None;
            *pending_restart = None;
        }
        Seat::Parked { .. } => {
            if let Seat::Parked { unit } = std::mem::replace(seat, Seat::InTransit) {
                log::info!("[plugin_host] engine slot {slot} unit {id}: retrying parked unit on restart ({why})");
                *seat = Seat::Owed { unit, activated_at: None };
            }
            *pending_restart = None;
        }
        Seat::Closing { .. } | Seat::Removing { .. } | Seat::InTransit => {
            *pending_restart = None;
        }
    }
}

/// Submit the unit held in `Seat::Owed`: with `host.terms()` present, reactivate at them unless
/// already activated at those exact terms (a box from the engine always has `activated_at: None`),
/// publish `host.activated(..)`, and call `host.submit(unit, fresh)`.
fn submit_owed<P: OwnedPlugin>(
    host: &UnitHost,
    plugin: &mut P,
    name: &str,
    seat: &mut Seat<P::Unit>,
    ever_seated: bool,
    answering_caller: bool,
) -> Option<Exit<P::Unit>> {
    let (mut unit, mut activated_at) = match std::mem::replace(seat, Seat::InTransit) {
        Seat::Owed { unit, activated_at } => (unit, activated_at),
        other => {
            *seat = other;
            return None;
        }
    };

    let Some(fresh) = host.terms() else {
        if !ever_seated && answering_caller {
            return Some(Exit::FailedBeforeSeat {
                unit: Some(unit),
                why: "no audio device is open".to_string(),
            });
        }
        *seat = Seat::Owed { unit, activated_at };
        return None;
    };

    if activated_at != Some(fresh) {
        match plugin.reactivate(unit, fresh) {
            Ok(rearmed) => {
                let info = rearmed.info();
                host.activated(Activated {
                    name: name.to_string(),
                    source: info.source,
                    latency: info.latency,
                });
                unit = rearmed;
                activated_at = Some(fresh);
            }
            Err((returned, why)) => {
                if !ever_seated {
                    return Some(Exit::FailedBeforeSeat { unit: Some(returned), why });
                }
                log::warn!(
                    "[plugin_host] engine slot {} unit {}: reactivate failed ({why}); parking",
                    host.slot(),
                    host.id()
                );
                host.parked(why);
                *seat = Seat::Parked { unit: returned };
                return None;
            }
        }
    }

    match host.submit(unit, fresh) {
        Ok(op) => {
            *seat = Seat::Installing { op };
            None
        }
        Err((returned, SubmitError::PortFull | SubmitError::Reloading)) => {
            *seat = Seat::Owed { unit: own(returned), activated_at };
            None
        }
        Err((returned, SubmitError::Stale)) => {
            *seat = Seat::Owed { unit: own(returned), activated_at: None };
            None
        }
        Err((returned, SubmitError::NoEngine)) => {
            let returned = own(returned);
            if !ever_seated && answering_caller {
                return Some(Exit::FailedBeforeSeat {
                    unit: Some(returned),
                    why: submit_error_reason(SubmitError::NoEngine),
                });
            }
            *seat = Seat::Owed { unit: returned, activated_at };
            None
        }
        Err((returned, SubmitError::Closing)) => {
            let returned = own(returned);
            if !ever_seated && answering_caller {
                return Some(Exit::FailedBeforeSeat {
                    unit: Some(returned),
                    why: submit_error_reason(SubmitError::Closing),
                });
            }
            *seat = Seat::Closing { unit: returned };
            None
        }
        Err((returned, SubmitError::Removing)) => {
            *seat = Seat::Removing { unit: own(returned) };
            None
        }
        Err((returned, SubmitError::Superseded)) => Some(Exit::Returned { unit: own(returned), retire: false }),
        Err((returned, SubmitError::Replaced)) => Some(Exit::Returned { unit: own(returned), retire: true }),
    }
}

/// End before the unit ever applied a seat: release the plugin and its unit (if held), delete the
/// files its add wrote (`undo_add`: the failed plate's removal has no owner left to retire them), fail
/// the plate, release the reservation, and then answer `Err(why)` so a caller that sees `Err` finds the
/// plugin already gone.
#[allow(clippy::too_many_arguments)]
fn fail_before_seat<P: OwnedPlugin>(
    host: UnitHost,
    mut plugin: P,
    unit: Option<Box<P::Unit>>,
    tone: &mut ToneKeeper,
    why: String,
    ready: &mut Option<SyncSender<Result<Answer, String>>>,
    faults: &AtomicU32,
    reported: &mut u32,
) -> Result<(), String> {
    let slot = host.slot();
    if plugin.editor() != Editor::Closed {
        plugin.editor_close();
    }
    plugin.release(unit);
    undo_add(&host, tone);
    report_faults(faults, slot, reported);
    host.failed(why.clone());
    host.release();
    if let Some(tx) = ready.take() {
        let _ = tx.try_send(Err(why));
    }
    Ok(())
}

/// A unit that never stood: delete the files its add wrote (`ToneKeeper::undo_add`) while its id is
/// still reserved, so a same-id add in the other slot (possible once `UnitHost::failed` or `release`
/// frees the id) never reads them as its own nor loses its own to this cleanup. A reload candidate's
/// redeemed tone is the seated unit's, and stays.
fn undo_add(host: &UnitHost, tone: &mut ToneKeeper) {
    if !host.is_reload() {
        tone.undo_add();
    }
}

/// How an owner whose unit is out of the engine ends its tone ([`ordered_release`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ToneEnd {
    /// The files stay as they are (a superseded instance; after a panic).
    Stay,
    /// The unit left for good: its tone retires into the slot's file, unless the host's shutdown's
    /// removal ended it (`UnitHost::keeps_tone`).
    Retire,
    /// An add whose first answer is still owed, so it answers `Err`: nothing is saved and the files the
    /// add wrote go ([`undo_add`]), whatever the shutdown's removal decided.
    Undo,
}

/// The tone end for a unit out of the engine: [`ToneEnd::Undo`] for an add (not a reload) whose
/// owner's first answer is still owed (`ready`), else retire or not as the terminal `Back` said.
fn tone_end(host: &UnitHost, ready: &Option<SyncSender<Result<Answer, String>>>, retire: bool) -> ToneEnd {
    if ready.is_some() && !host.is_reload() {
        ToneEnd::Undo
    } else if retire {
        ToneEnd::Retire
    } else {
        ToneEnd::Stay
    }
}

/// Ordered release once the unit is out of the engine (or none is held after `Dismissed`): close any
/// editor → fold a last `turn` into the tone → `quiesce` → save if dirty or editor was open → end the
/// tone as `end` says ([`ToneEnd`]) → `release(unit)` → `host.release()`. Skips `turn`, save and the
/// tone's end after a panic, and `turn` and save for [`ToneEnd::Undo`].
#[allow(clippy::too_many_arguments)]
fn ordered_release<P: OwnedPlugin>(
    host: UnitHost,
    mut plugin: P,
    unit: Option<Box<P::Unit>>,
    tone: &mut ToneKeeper,
    name: &str,
    allow_save: bool,
    end: ToneEnd,
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
    let save = allow_save && end != ToneEnd::Undo;
    if save {
        let Reported { relist, changed } = plugin.turn(&(Arc::new(|_| {}) as EventSink));
        if relist || changed {
            tone.note_change(Instant::now());
        }
    }
    let keep = save && (editor_open || tone.dirty());
    plugin.quiesce();
    if keep {
        let _ = keep_tone(tone, slot, "unload", name, || plugin.state());
    }
    // When the host's shutdown's removal is what ended the unit, its own file stays for the next launch's
    // restore under the same id (`EngineHost::remove_unit_at_shutdown`, decided with the terminal `Back`);
    // any other end (the player's removal, a source replacement) retires it into the slot's file.
    match end {
        ToneEnd::Retire if allow_save && !host.keeps_tone() => tone.retire(),
        ToneEnd::Undo if allow_save => tone.undo_add(),
        ToneEnd::Retire | ToneEnd::Undo | ToneEnd::Stay => {}
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
        undo_add(&host, &mut tone);
        host.failed(why.clone());
        host.release();
        if let Some(tx) = ready.take() {
            let _ = tx.try_send(Err(why));
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
            undo_add(&host, &mut tone);
            host.failed(e.clone());
            host.release();
            if let Some(tx) = ready.take() {
                let _ = tx.try_send(Err(e));
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
            &mut tone,
            "plugin load cancelled".to_string(),
            &mut ready,
            &faults,
            &mut reported,
        );
    }

    let mut seat = Seat::Owed { unit, activated_at: Some(terms) };
    let mut ever_seated = false;
    let mut pending_restart: Option<String> = None;
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
                        ever_seated = true;
                        seat = Seat::Owed { unit: own(unit), activated_at: None };
                        pending_restart = None;
                    }
                    Back::Evicted { unit, .. } => {
                        if matches!(seat, Seat::Installing { op } if host.poll(op) == OpState::Applied) {
                            ever_seated = true;
                            send_answer(&mut ready, slot, id, Answer::Seated { name: name.clone(), tone: restored });
                        }
                        seat = Seat::Owed { unit: own(unit), activated_at: None };
                        pending_restart = None;
                    }
                }
            }

            note_restart(&host, &mut plugin, &mut seat, &mut pending_restart);

            if matches!(seat, Seat::Owed { .. }) {
                if let Some(exit) =
                    submit_owed(&host, &mut plugin, &name, &mut seat, ever_seated, ready.is_some())
                {
                    return exit;
                }
            }

            match seat {
                Seat::Installing { op } => match host.poll(op) {
                    OpState::Applied => {
                        seat = Seat::Seated;
                        ever_seated = true;
                        send_answer(&mut ready, slot, id, Answer::Seated { name: name.clone(), tone: restored });
                        apply_pending_restart(&host, &mut seat, &mut pending_restart);
                    }
                    OpState::Held => {
                        send_answer(&mut ready, slot, id, Answer::Waiting { name: name.clone(), tone: restored });
                    }
                    OpState::Pending if host.held() => {
                        send_answer(&mut ready, slot, id, Answer::Waiting { name: name.clone(), tone: restored });
                    }
                    OpState::Pending | OpState::Refused(_) | OpState::Rebuilt => {}
                },
                Seat::Withdrawing { op } => {
                    let _ = host.poll(op);
                }
                _ => {}
            }

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
            note_restart(&host, &mut plugin, &mut seat, &mut pending_restart);
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
                note_restart(&host, &mut plugin, &mut seat, &mut pending_restart);
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
            fail_before_seat(host, plugin, unit, &mut tone, why, &mut ready, &faults, &mut reported)
        }
        Ok(Exit::Returned { unit, retire }) => {
            let end = tone_end(&host, &ready, retire);
            let res = ordered_release(host, plugin, Some(unit), &mut tone, &name, true, end, &faults, &mut reported);
            if let Some(tx) = ready.take() {
                let _ = tx.try_send(Err("unit left before seating".to_string()));
            }
            res
        }
        Ok(Exit::Dismissed) => {
            let unit = seat.take_unit();
            let end = tone_end(&host, &ready, true);
            let res = ordered_release(host, plugin, unit, &mut tone, &name, true, end, &faults, &mut reported);
            if let Some(tx) = ready.take() {
                let _ = tx.try_send(Err("unit dismissed before seating".to_string()));
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
            let pending_op = match seat {
                Seat::Installing { op } | Seat::Withdrawing { op } => Some(op),
                _ => None,
            };
            let mut in_hand = seat.take_unit();
            let mut retire = false;
            let mut dismissed = false;
            let drain_shutdown = |in_hand: &mut Option<Box<P::Unit>>, retire: &mut bool, dismissed: &mut bool| {
                while let Some(back) = host.take_back_for_shutdown() {
                    match back {
                        Back::Removed(u) => {
                            *in_hand = Some(own(u));
                            *retire = true;
                        }
                        Back::Superseded(u) => {
                            *in_hand = Some(own(u));
                            *retire = false;
                        }
                        Back::Withdrawn { unit: u, .. }
                        | Back::Evicted { unit: u, .. }
                        | Back::Refused { unit: u, .. } => {
                            *in_hand = Some(own(u));
                        }
                        Back::Dismissed => {
                            *retire = true;
                            *dismissed = true;
                        }
                    }
                }
            };
            if let Some(op) = pending_op {
                let _ = host.poll(op);
            }
            drain_shutdown(&mut in_hand, &mut retire, &mut dismissed);
            if in_hand.is_none() && !dismissed && host.terms().is_some() && host.removal_in_flight() {
                let deadline = Instant::now() + STOP_RETURN_TIMEOUT;
                while in_hand.is_none() && !dismissed && Instant::now() < deadline {
                    if let Some(op) = pending_op {
                        let _ = host.poll(op);
                    }
                    drain_shutdown(&mut in_hand, &mut retire, &mut dismissed);
                    if in_hand.is_some() || dismissed || host.terms().is_none() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                drain_shutdown(&mut in_hand, &mut retire, &mut dismissed);
            }
            // A unit out of the engine (including a dismissed seat, whose box is already in hand or
            // ahead of `Dismissed` in the mailbox, or a box held in `Leave::Pending` during a
            // pending removal) takes the ordered release.
            if in_hand.is_some() || dismissed {
                let end = tone_end(&host, &ready, retire);
                ordered_release(host, plugin, in_hand, &mut tone, &name, !panicked, end, &faults, &mut reported)
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
    spawn_unit_with(format, unit, editor_parent, sink, tone, run, |name, f| {
        std::thread::Builder::new().name(name).spawn(f)
    })
}

pub(super) fn spawn_unit_with(
    format: PluginFormat,
    unit: UnitHost,
    editor_parent: usize,
    sink: EventSink,
    tone: Option<ToneBinding>,
    run: impl FnOnce(UnitCtx) -> Result<(), String> + Send + 'static,
    spawn: impl FnOnce(String, Box<dyn FnOnce() -> Result<(), String> + Send>) -> std::io::Result<JoinHandle<Result<(), String>>>,
) -> Result<(UnitOwner, Answer), String> {
    let slot = unit.slot();
    let id = unit.id();
    let running = Arc::new(AtomicBool::new(true));
    let claim = Arc::new(LoadClaim::default());
    let edited = Arc::new(AtomicBool::new(false));
    let (params_tx, params) = RingBuffer::<ParamEvent>::new(EVENT_RING_CAP);
    let params_tx = Arc::new(Mutex::new(params_tx));
    let (request_tx, requests) = std::sync::mpsc::channel();
    let (ready, ready_rx) = ready_channel();
    let param_ids = ParamIds::default();
    let shared_name = Arc::new(OnceLock::new());
    let on_spawn_err = unit.clone();
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
    let owner = match spawn(format!("lf-{}-unit-{id}", format.as_str()), Box::new(move || run(ctx))) {
        Ok(owner) => {
            drop(on_spawn_err);
            owner
        }
        Err(e) => {
            let why = format!("spawn unit owner thread: {e}");
            on_spawn_err.failed(why.clone());
            on_spawn_err.release();
            return Err(why);
        }
    };

    let build_handle = |name: String, owner: JoinHandle<Result<(), String>>| UnitOwner {
        slot,
        id,
        format,
        name,
        running: running.clone(),
        owner: Mutex::new(Some(owner)),
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
    format: PluginFormat,
    name: String,
    running: Arc<AtomicBool>,
    owner: Mutex<Option<JoinHandle<Result<(), String>>>>,
    requests: Sender<OwnerRequest>,
    params_tx: Arc<Mutex<Producer<ParamEvent>>>,
    param_ids: ParamIds,
    /// The owner's `ToneKeeper` flag: a host parameter set is a change to save.
    edited: Arc<AtomicBool>,
}

#[allow(dead_code)] // the chain commands (P4) call these; the tests call them now
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

    /// Set a parameter the plugin listed: the unit applies it at the start of the next block it
    /// renders, and a VST3 edit controller is told as well (`src-tauri/AGENTS.md` § Plugin hosting,
    /// "Host-set VST3 params go to BOTH halves"). Err for an id the plugin never listed (an unknown id
    /// can crash a plugin) or a full ring; the controller only hears what the processor got. The
    /// ring's lock is held through the controller's request too, so two sets from two threads reach
    /// both halves in the same order.
    pub(crate) fn set_param(&self, id: u32, value: f64) -> Result<(), String> {
        let unit = self.id;
        let known = self.param_ids.read().map(|set| set.contains(&id)).unwrap_or(false);
        if !known {
            return Err(format!("parameter {id} is not one the plugin of unit {unit} listed"));
        }
        let mut ring = self
            .params_tx
            .lock()
            .map_err(|_| format!("unit {unit} event queue is unavailable (lock poisoned)"))?;
        if ring.push(ParamEvent { id, value }).is_err() {
            return Err(format!("unit {unit} event queue is full; parameter {id} change was dropped"));
        }
        self.edited.store(true, Release);
        if self.format == PluginFormat::Vst3 && self.requests.send(OwnerRequest::SetParamNormalized(id, value)).is_err() {
            return Err(format!("unit {unit} owner thread gone; parameter {id} reached the processor, not the controller"));
        }
        Ok(())
    }

    /// Ask for a tone save without waiting (the app's exit waits on every reply under one deadline).
    pub(crate) fn start_tone_save(&self) -> Result<Receiver<Result<Vec<u8>, String>>, String> {
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        self.requests
            .send(OwnerRequest::SaveTone(reply_tx))
            .map_err(|_| "unit owner thread gone".to_string())?;
        Ok(reply_rx)
    }

    /// Open the plugin's editor (≤ 5 s; an editor that comes up later is closed again).
    pub(crate) fn open_editor(&self) -> Result<(), String> {
        owner_request_5s(&self.requests, "open_editor", OwnerRequest::OpenEditor)
    }

    pub(crate) fn close_editor(&self) -> Result<(), String> {
        owner_request_5s(&self.requests, "close_editor", OwnerRequest::CloseEditor)
    }

    /// Test-only: a stop's first step alone: the owner's loop ends at its next check, unwoken, and
    /// `stop` still joins it.
    #[cfg(test)]
    pub(super) fn stop_serving(&self) {
        self.running.store(false, Release);
    }

    /// Whether the owner thread has exited (e.g. after `Back::Removed` tore it down).
    pub(crate) fn is_finished(&self) -> bool {
        let owner = self.owner.lock().unwrap_or_else(|e| e.into_inner());
        owner.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Signal the owner loop to exit and wake it without blocking.
    pub(crate) fn request_stop(&self) {
        self.running.store(false, Release);
        let _ = self.requests.send(OwnerRequest::Wake);
    }

    /// Wait for the owner thread to exit, if it has not already been joined.
    pub(crate) fn join(&self) -> Result<(), String> {
        let Some(owner) = self.owner.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return Ok(());
        };
        let t = Instant::now();
        let result = owner.join().unwrap_or_else(|_| Err("the unit owner thread panicked".to_string()));
        log::info!(
            "[plugin_host] engine slot {} unit {} owner joined in {} ms",
            self.slot,
            self.id,
            t.elapsed().as_millis()
        );
        result
    }

    /// Ask the owner loop to exit, wake it, and join the thread.
    pub(crate) fn stop(self) -> Result<(), String> {
        self.shutdown()
    }

    fn shutdown(&self) -> Result<(), String> {
        let Some(owner) = self.owner.lock().unwrap_or_else(|e| e.into_inner()).take() else {
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

#[cfg(test)]
pub(crate) mod test_support {
    use std::any::Any;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use lf_engine::grid::Frame;
    use lf_engine::{SlotEvent, UnitCx, UnitInfo, UnitKind, UnitParam, UnitProcessor};

    use super::{serve_unit, spawn_unit, Answer, EventSink, Loaded, OwnedPlugin, PluginFormat, Reported, Setup, Terms, ToneBinding, UnitHost, UnitOwner};
    use crate::engine_io::SlotHost;
    use crate::host::engine_slot::Editor;
    use crate::host::ParamDesc;

    #[derive(Clone)]
    pub(crate) struct FakePluginConfig {
        pub(crate) name: String,
        pub(crate) source: bool,
        pub(crate) latency: Frame,
        pub(crate) state_bytes: Vec<u8>,
        pub(crate) on_state: Option<Arc<dyn Fn() + Send + Sync>>,
        pub(crate) on_reactivate: Option<Arc<dyn Fn() + Send + Sync>>,
        pub(crate) on_release: Option<Arc<dyn Fn() + Send + Sync>>,
        /// Called at every owner turn (`OwnedPlugin::turn`): a test may hold the owner's loop there.
        pub(crate) on_turn: Option<Arc<dyn Fn() + Send + Sync>>,
        /// Called inside the load, after its tone read (`restores`), before the owner claims and submits.
        pub(crate) on_load: Option<Arc<dyn Fn() + Send + Sync>>,
        pub(crate) released_count: Option<Arc<AtomicUsize>>,
        pub(crate) exited_flag: Option<Arc<AtomicBool>>,
        /// When set, the load reads its tone as a real plugin's load does (`ToneKeeper::stored`) and
        /// records the state it would restore (`None`: nothing stored or unreadable).
        pub(crate) restores: Option<Arc<Mutex<Vec<Option<Vec<u8>>>>>>,
    }

    impl Default for FakePluginConfig {
        fn default() -> Self {
            Self {
                name: "FakePlugin".to_string(),
                source: false,
                latency: 0,
                state_bytes: b"fake-tone".to_vec(),
                on_state: None,
                on_reactivate: None,
                on_release: None,
                on_turn: None,
                on_load: None,
                released_count: None,
                exited_flag: None,
                restores: None,
            }
        }
    }

    pub(crate) struct FakePluginUnit {
        source: bool,
        latency: Frame,
    }

    impl UnitProcessor for FakePluginUnit {
        fn info(&self) -> UnitInfo {
            UnitInfo { kind: UnitKind::Plugin, source: self.source, latency: self.latency }
        }

        fn process(&mut self, _cx: &UnitCx, input: [&[f32]; 2], _events: &[SlotEvent], out: [&mut [f32]; 2]) {
            for (o, i) in out.into_iter().zip(input) {
                o.copy_from_slice(i);
            }
        }

        fn set_param(&mut self, _cx: &UnitCx, _param: UnitParam, _value: f64) -> Option<f64> {
            None
        }

        fn stop(&mut self) {}

        fn into_any(self: Box<Self>) -> Box<dyn Any + Send> {
            self
        }
    }

    struct FakeOwnedPlugin {
        cfg: FakePluginConfig,
    }

    impl OwnedPlugin for FakeOwnedPlugin {
        type Unit = FakePluginUnit;
        const FORMAT: &'static str = "CLAP";

        fn take_restart(&mut self) -> Option<String> {
            None
        }

        fn reinstall(&mut self, _unit: Box<Self::Unit>, _slot: &SlotHost) -> Result<(), (Box<Self::Unit>, String)> {
            unreachable!("reinstall is legacy-only")
        }

        fn reactivate(
            &mut self,
            unit: Box<Self::Unit>,
            _terms: Terms,
        ) -> Result<Box<Self::Unit>, (Box<Self::Unit>, String)> {
            if let Some(cb) = &self.cfg.on_reactivate {
                cb();
            }
            Ok(unit)
        }

        fn turn(&mut self, _sink: &EventSink) -> Reported {
            if let Some(cb) = &self.cfg.on_turn {
                cb();
            }
            Reported::default()
        }

        fn idle(&mut self) -> bool {
            false
        }

        fn params(&mut self) -> Result<Vec<ParamDesc>, String> {
            Ok(Vec::new())
        }

        fn state(&mut self) -> Result<Option<Vec<u8>>, String> {
            if let Some(cb) = &self.cfg.on_state {
                cb();
            }
            Ok(Some(self.cfg.state_bytes.clone()))
        }

        fn editor(&self) -> Editor {
            Editor::Closed
        }

        fn editor_open(&mut self) -> Result<(), String> {
            Ok(())
        }

        fn editor_close(&mut self) {}

        fn release(self, unit: Option<Box<Self::Unit>>) {
            if let Some(cb) = &self.cfg.on_release {
                cb();
            }
            drop(unit);
            if let Some(cnt) = &self.cfg.released_count {
                cnt.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    pub(crate) fn spawn_fake_unit(
        unit_host: UnitHost,
        sink: EventSink,
        tone: Option<ToneBinding>,
        cfg: FakePluginConfig,
    ) -> Result<(UnitOwner, Answer), String> {
        let exited = cfg.exited_flag.clone();
        spawn_unit(PluginFormat::Clap, unit_host, 0, sink, tone, move |ctx| {
            let res = serve_unit(ctx, move |setup: Setup<'_>| {
                if let Some(restores) = &cfg.restores {
                    let stored = setup.tone.stored().ok().flatten();
                    restores.lock().unwrap_or_else(|e| e.into_inner()).push(stored);
                }
                if let Some(cb) = &cfg.on_load {
                    cb();
                }
                let rate = setup.slot.rate().unwrap_or(0);
                let unit = Box::new(FakePluginUnit { source: cfg.source, latency: cfg.latency });
                let name = cfg.name.clone();
                Ok(Loaded { plugin: FakeOwnedPlugin { cfg }, unit, name, rate, tone: None })
            });
            if let Some(flag) = exited {
                flag.store(true, Ordering::SeqCst);
            }
            res
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::test_support::{spawn_fake_unit, FakePluginConfig};
    use super::Answer;
    use crate::engine_io::chain::PluginRef;
    use crate::engine_io::test_rig::TestDevice;
    use crate::engine_io::unit_host::Spec;
    use crate::host::tone::{ToneBinding, ToneKey, ToneStore};

    /// P4 fix r2 item 6: an evicted source superseded by a newer source after a rebuild retires its
    /// tone (D1: replaced source promotes its unit file to the slot file), while the newer source
    /// stays seated.
    #[test]
    fn p4f2_6_evicted_source_replaced_after_rebuild_retires_tone_and_keeps_newer_source() {
        let device = TestDevice::start(48_000, 256, Duration::from_millis(2), |_| 0.0);
        let host = device.host().clone();

        let dir = std::env::temp_dir().join(format!(
            "bleeploop-p4f2-item6-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = ToneStore::new(dir.clone());

        let synth_a = PluginRef {
            format: "clap".into(),
            path: "C:\\plugins\\SynthA.clap".into(),
            id: "synth.a".into(),
        };
        let synth_b = PluginRef {
            format: "clap".into(),
            path: "C:\\plugins\\SynthB.clap".into(),
            id: "synth.b".into(),
        };

        let (in_reactivate_tx, in_reactivate_rx) = std::sync::mpsc::sync_channel::<()>(1);
        let (resume_reactivate_tx, resume_reactivate_rx) = std::sync::mpsc::sync_channel::<()>(1);
        let resume_reactivate_rx = Arc::new(std::sync::Mutex::new(Some(resume_reactivate_rx)));
        let a_exited = Arc::new(AtomicBool::new(false));
        let paused_once = Arc::new(AtomicBool::new(false));

        let uh_a = host.unit(0, 610, Spec::Plugin(synth_a.clone()), 0, false).unwrap();
        let binding_a = ToneBinding {
            store: store.clone(),
            key: ToneKey::Unit { unit: 610, slot: 0, identity: synth_a.clone() },
            imported: None,
        };
        let p_once = paused_once.clone();
        let (owner_a, ans_a) = spawn_fake_unit(
            uh_a,
            Arc::new(|_| {}),
            Some(binding_a),
            FakePluginConfig {
                name: "SynthA".into(),
                source: true,
                state_bytes: b"synth-a-dialed-in".to_vec(),
                on_reactivate: Some(Arc::new(move || {
                    if !p_once.swap(true, Ordering::AcqRel) {
                        let _ = in_reactivate_tx.try_send(());
                        if let Some(rx) = resume_reactivate_rx.lock().unwrap_or_else(|e| e.into_inner()).take() {
                            let _ = rx.recv_timeout(Duration::from_secs(10));
                        }
                    }
                })),
                exited_flag: Some(a_exited.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(matches!(ans_a, Answer::Seated { .. }), "got {ans_a:?}");

        // Save A's tone so its unit file exists on disk while the slot file does not.
        let saved_bytes = owner_a.take_tone().unwrap();
        assert!(!saved_bytes.is_empty());
        assert!(dir.join("unit-610.tone").exists(), "unit-610.tone should exist before rebuild");
        assert_eq!(store.load(0, &synth_a).unwrap(), None, "slot file should be empty before retire");

        // Rebuild: A is evicted and enters reactivate, pausing before its retry submit.
        device.rebuild_at(96_000, 512);
        in_reactivate_rx
            .recv_timeout(crate::engine_io::test_rig::PATIENCE)
            .expect("owner A did not enter reactivate after rebuild");

        // While A is paused in reactivate, seat newer source B in slot 0.
        let uh_b = host.unit(0, 611, Spec::Plugin(synth_b.clone()), 0, false).unwrap();
        let (owner_b, ans_b) = spawn_fake_unit(
            uh_b,
            Arc::new(|_| {}),
            None,
            FakePluginConfig {
                name: "SynthB".into(),
                source: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(matches!(ans_b, Answer::Seated { .. }), "got {ans_b:?}");

        // Resume A so it submits against the new engine while newer source B heads slot 0.
        let _ = resume_reactivate_tx.try_send(());

        // Wait for owner A to exit on its own (no stop requested).
        let deadline = Instant::now() + crate::engine_io::test_rig::PATIENCE;
        while !a_exited.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "owner A did not exit after being replaced");
            std::thread::sleep(Duration::from_millis(5));
        }

        // Newer source B stays seated in slot 0.
        let chain_ids: Vec<u32> = {
            let rt = host.core.rt.lock().unwrap();
            rt.engine.as_ref().unwrap().chain(0).map(|s| s.unit).collect()
        };
        assert_eq!(chain_ids, vec![611], "newer source B must stay seated in slot 0");

        // Old owner A's tone MUST be retired (unit file promoted to slot 0 file).
        assert!(
            !dir.join("unit-610.tone").exists(),
            "replaced evicted source A must retire its unit-610.tone file"
        );
        let promoted = store
            .load(0, &synth_a)
            .unwrap()
            .expect("replaced evicted source A's tone must be promoted to slot 0 file");
        assert_eq!(promoted.state, b"synth-a-dialed-in");

        let _ = host.remove_unit(611);
        host.shutdown();
        let _ = owner_a.stop();
        let _ = owner_b.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
