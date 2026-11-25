//! OWNS: chain mode's rig record on disk, one [`Rig`] per process (in `EngineApp`): its files and their
//! lock (D8), the page's handover (`rig_open`, `rig_import`, `rig_seen`; D13), the writer's 100 ms tick
//! and the close at shutdown (D7). The rules are the pure crate `crates/lf-rig` (its crate doc); this is
//! the Windows glue that does the I/O and drives them. `disk` is the data folder ([`lf_rig::Disk`]),
//! `writer` the tick and the close.
//!
//! **Who writes.** Only this `Rig` writes the record, and only the restore's `CommitMarker` the crash
//! marker; the page writes neither. The record changes in three places: the page's import (once, while
//! no record exists), the writer's tick on the rig thread (CP3a's restore commits there too, one at a
//! time with the ticks), and the close, after which nothing is written.
//!
//! **Every durable write is [`lf_rig::project()`]'s output over a fresh view.** Each tick and the close
//! take the settings' slot gains (D2), then [`Model::rig_view`](super::chain::Model::rig_view) under
//! `state`, each lock released before the next, then project under `inner`. The glue never hands the
//! store an older projection after a newer commit: `Store::tick(old)` after `commit(new)` would store a
//! state the chain already left. The ticks and the restore's commits run on the one rig thread in
//! order, the import writes only while no record exists (no projection is owed then), and close sets
//! `closing` before it takes its copy, which every later write checks under `disk`: a tick that took
//! its view before close's copy and reaches `disk` after it writes nothing. The import checks its
//! page's epoch again under `disk` right before its commit: a page replaced meanwhile stores nothing.
//!
//! **Close** ([`Rig::close`], D7) decides under `disk` whether it writes: the store's status there,
//! not `inner`'s copy, which an import or a first open that committed may not have published yet (close
//! takes the store's record and publishes it). Before its copy it waits, bounded, for one completed feed
//! pass, so an install or a parameter the engine applied and the event ring still holds reaches the
//! model first; it never drains the feed itself. A second close (the updater's shutdown, then the
//! exit's) returns only once the first has written and released the lock file, and does nothing. Once
//! `closing` is set no open opens or reads the file.
//!
//! **Restore then project, same view (CP3a).** Once the restore runs, each tick feeds
//! `Restore::handle(ViewChanged)` and `project` the same fresh `rig_view`, the restore first, so the two
//! never judge an id apart ([`lf_rig::Plan::fate`]); a view taken before an add's answer is never fed
//! after it (its plate would read as removed). Until CP3a wires the restore, the plan is the record as
//! found or imported, made at a view ([`held_plan`]): a record unit with an applied plate under its id
//! is `Matched` to that plate (the player's removal of it ends it), every other unit `Held` at its
//! place, so the writer keeps every record unit the model lacks instead of writing them away, and adds
//! what the player applies. Each projection first drops the ids the plan's rule ends at that view
//! ([`end_ids`]); close judges them at its full view before it drops the `Leaving` plates.
//!
//! **Locks.** `inner` (the plan, the stored record's copy the projection reads, the notice) and `disk`
//! (the store, so one write at a time, and `closed`, the done flag a second close waits on). Neither is
//! held across a `settings` or `state` take, a scan, an add or a wait (a second close's wait on
//! `closed_cv` releases `disk`), nor are the two held together; `disk` is held across one
//! `write_atomic` at most. A record write is never made under `state` or `settings`.
//!
//! **Release.** The `rig_*` commands are DEV-only (`lib.rs`), in lockstep with `lf.chainNative`, so a
//! release build opens no rig and runs no writer before activation; its shutdown still calls
//! [`Rig::close`], which then has nothing to write.

mod disk;
#[cfg(test)]
mod tests;
mod writer;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::{AcqRel, Acquire, Relaxed}};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use lf_engine::UnitId;
use lf_rig::{Commit, Disposition, Fate, Files, ImportError, Notice, NoticeId, Opened, Plan, Refusal, RigAnswer, RigRecord, RigStatus, RigView, Store, Volumes, IMPORT_STALE};

use super::mode::{engine, EngineApp};
use super::EngineHost;
use crate::host::PluginHostState;
pub(crate) use disk::files;
use disk::FileDisk;

/// The process's rig record (the module doc).
pub(crate) struct Rig {
    /// The app-local data folder; `None` without one (every open answers `unavailable`).
    dir: Option<PathBuf>,
    files: Files,
    /// Set first by [`Rig::close`] (D7): no write starts after it. Every write checks it under `disk`.
    closing: AtomicBool,
    /// The writer thread was started (once per process).
    writer: AtomicBool,
    inner: Mutex<Inner>,
    disk: Mutex<DiskState>,
    /// Signalled under `disk` when the first close sets `closed` (a second close waits on it).
    closed_cv: Condvar,
}

/// What the projection reads, under `inner`.
#[derive(Default)]
struct Inner {
    /// What the record file holds, as the store last read or committed it (`project`'s `record`), and
    /// the store's write count it was copied at, so a slower copy never replaces a newer one.
    stored: Option<RigRecord>,
    stored_at: u64,
    /// The plan was made from the first record this process found or imported.
    planned: bool,
    plan: Plan,
    /// What every open answers until `rig_seen` (D13); the restore's (CP3a).
    notice: Option<Notice>,
}

/// The store, under `disk`.
#[derive(Default)]
struct DiskState {
    /// `None` before the first open, and after close (dropped: its lock file is released).
    store: Option<Store<FileDisk>>,
    /// Close finished (its write made, the lock file released): nothing is opened or written again.
    closed: bool,
    /// Bumped at the open and at each commit: orders the copies `inner` keeps.
    at: u64,
}

/// What a step under `disk` found, for `inner` and the answer.
struct Seen {
    status: RigStatus,
    stored: Option<RigRecord>,
    at: u64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Rig {
    pub(crate) fn new(dir: Option<PathBuf>, files: Files) -> Rig {
        Rig {
            dir,
            files,
            closing: AtomicBool::new(false),
            writer: AtomicBool::new(false),
            inner: Mutex::default(),
            disk: Mutex::default(),
            closed_cv: Condvar::new(),
        }
    }

    /// `rig_open` (D13): what the record is for this process, each slot's volume the fader shows (the
    /// settings' slot gain, else the record's: D2) and the notice. The first call opens the store: the
    /// lock, then the read (D8). The restore starts here from CP3a; a later call answers the same facts.
    pub(crate) fn open(&self, host: &EngineHost) -> RigAnswer {
        let held = writer::slot_gains(host);
        let seen = self.opened();
        let view = writer::view(host);
        self.keep(&seen, Some(&view));
        self.answer(&seen, held)
    }

    /// `rig_import` (D12, D13): the page's upgrade of the legacy keys, stored only while no record
    /// exists (else answered `present`, nothing stored). Each slot whose model holds applied units takes
    /// them from a fresh view in place of the import's (DEV adoption); an import id the model holds
    /// refuses it. Its commit is its durability: a failed write is an `Err`, and the next launch answers
    /// `absent`. A replaced page's import is `Err("stale")`, checked at the call and again under `disk`
    /// right before the commit (a `host_init` meanwhile stores nothing).
    pub(crate) fn import(&self, host: &EngineHost, doc: (&PluginHostState, u32), text: &str) -> Result<RigAnswer, String> {
        let stale = || doc.0.frontend_epoch.load(Relaxed) != doc.1;
        if stale() {
            return Err(IMPORT_STALE.to_string());
        }
        let seen = self.opened();
        if seen.status != RigStatus::Absent {
            let held = writer::slot_gains(host);
            self.keep(&seen, Some(&writer::view(host)));
            return Ok(self.answer(&seen, held));
        }
        self.keep(&seen, None);
        let held = writer::slot_gains(host);
        let view = writer::view(host);
        let record = lf_rig::import(text, &view, held).map_err(|e| match e {
            ImportError::Unreadable(u) => format!("the import is not a rig record: {}", u.why),
            ImportError::IdHeld(id) => format!("the import's unit {id} is in the chain already"),
        })?;
        let (commit, seen) = {
            let mut disk = lock(&self.disk);
            if self.closing.load(Acquire) || disk.closed {
                return Err("the rig is closing".to_string());
            }
            if stale() {
                return Err(IMPORT_STALE.to_string());
            }
            let commit = match disk.store.as_mut() {
                Some(store) => store.import(&record),
                None => Commit::Refused(Refusal::Closed),
            };
            if commit == Commit::Committed {
                disk.at += 1;
            }
            (commit, Self::found(&disk))
        };
        // The plan is made at the adoption's view: an adopted unit is matched to its plate.
        self.keep(&seen, Some(&view));
        match commit {
            Commit::Committed | Commit::Refused(Refusal::Present | Refusal::Unreadable | Refusal::Unavailable) => Ok(self.answer(&seen, held)),
            Commit::Failed { why, .. } => {
                log::warn!("[rig] the import was not stored: {why}");
                Err(format!("the rig record was not stored: {why}"))
            }
            Commit::Refused(why) => Err(format!("the rig record was not stored ({why:?})")),
        }
    }

    /// `rig_seen` (D13): the page showed notice `id`; every later open answers none.
    pub(crate) fn seen(&self, id: NoticeId) {
        let mut inner = lock(&self.inner);
        if inner.notice.as_ref().is_some_and(|n| n.id == id) {
            inner.notice = None;
        }
    }

    /// The store, opened by the first call (the lock, then the read), as it stands. Once `closing` is
    /// set nothing is opened or read: an open that comes first then answers `unavailable` (the status
    /// with no store, as after close: this process writes no record, and the page imports nothing).
    fn opened(&self) -> Seen {
        let mut disk = lock(&self.disk);
        if disk.store.is_none() && !disk.closed && !self.closing.load(Acquire) {
            let (store, opened) = Store::open(FileDisk::new(self.dir.clone()), self.files);
            match &opened {
                Opened::Unavailable { why } => log::warn!("[rig] the rig record is unavailable this launch: {why}"),
                Opened::Unreadable(u) => log::warn!("[rig] the rig record is unreadable; it is kept and nothing is written this launch: {}", u.why),
                Opened::Present(_) | Opened::Absent => {}
            }
            disk.store = Some(store);
            disk.at += 1;
        }
        Self::found(&disk)
    }

    fn found(disk: &DiskState) -> Seen {
        match &disk.store {
            Some(store) => Seen { status: store.status(), stored: store.stored().cloned(), at: disk.at },
            None => Seen { status: RigStatus::Unavailable, stored: None, at: disk.at },
        }
    }

    /// Keep the store's copy in `inner` unless a newer one is there; the first record the process
    /// finds or imports makes the plan, at `view` (taken after the store's copy, each lock released).
    /// Every call that can first see a record passes its view; a tick's (after a commit) need not.
    fn keep(&self, seen: &Seen, view: Option<&RigView>) {
        let mut inner = lock(&self.inner);
        if seen.at >= inner.stored_at {
            inner.stored = seen.stored.clone();
            inner.stored_at = seen.at;
        }
        if !inner.planned {
            if let (Some(record), Some(view)) = (&seen.stored, view) {
                inner.plan = held_plan(record, view);
                inner.planned = true;
            }
        }
    }

    fn answer(&self, seen: &Seen, held: Volumes) -> RigAnswer {
        let volumes = match (&seen.status, &seen.stored) {
            (RigStatus::Present, Some(record)) => Some(lf_rig::volumes(record, held)),
            _ => None,
        };
        RigAnswer { status: seen.status, volumes, notice: lock(&self.inner).notice.clone() }
    }

    /// Start the writer's tick on the rig thread, once per process (the first `rig_open` or
    /// `rig_import`).
    pub(crate) fn start_writer(self: &Arc<Self>, host: EngineHost) {
        if self.writer.swap(true, AcqRel) {
            return;
        }
        let rig = self.clone();
        let spawned = std::thread::Builder::new().name("lf-rig".into()).spawn(move || writer::run(&rig, &host));
        if let Err(e) = spawned {
            log::error!("[rig] no thread for the rig record's writer ({e}); changes are stored at close only");
        }
    }
}

/// The plan until CP3a's restore makes it: the record's units and `pending` at their places. A unit
/// with an applied plate under its id at `view` (a DEV import's adopted unit, a unit the model held when
/// the record was found) is `Matched` to that plate, its token seen, so [`Plan::fate`] ends it at the
/// player's removal like any matched unit, also one made before the next view; every other unit is
/// `Held` (not restored this launch), so the projection keeps every record unit the model lacks. A plate
/// that has not applied matches nothing: its failed add must not end the record's unit.
pub(crate) fn held_plan(record: &RigRecord, view: &RigView) -> Plan {
    let mut plan = Plan::default();
    for (s, slot) in record.slots.iter().enumerate() {
        for unit in &slot.units {
            let applied = view.slots.iter().flatten().find(|p| p.id == unit.id && p.applied);
            let disposition = match applied {
                Some(plate) => {
                    plan.seen.insert(plate.added_by);
                    Disposition::Matched { token: plate.added_by }
                }
                None => Disposition::Held { why: "the restore is not wired yet".into(), token: None },
            };
            plan.dispositions.insert(unit.id, disposition);
        }
        plan.units[s] = slot.units.clone();
        plan.pending[s] = slot.pending.clone();
    }
    plan
}

/// Drop from the plan every id whose fate at `view` is `Ended` (the player's applied unit won it), as
/// the restore does at each view, so a unit the player removed later does not come back from the plan.
fn end_ids(plan: &mut Plan, view: &RigView) {
    let ended: Vec<UnitId> = plan.ids().into_iter().filter(|&id| plan.fate(id, view) == Some(Fate::Ended)).collect();
    for id in ended {
        plan.dispositions.remove(&id);
        for units in plan.units.iter_mut().chain(plan.pending.iter_mut()) {
            units.retain(|u| u.id != id);
        }
    }
}

/// A rig command's work on the runtime's blocking pool (its disk I/O there, D13).
async fn blocking<T: Send + 'static>(op: &'static str, work: impl FnOnce(&'static EngineApp) -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(move || work(engine()?)).await.map_err(|e| format!("{op} task: {e}"))?
}

/// What the native rig record is for this process (D13); the first call opens it and starts the writer.
#[tauri::command]
pub async fn rig_open() -> Result<RigAnswer, String> {
    blocking("rig_open", |app| {
        let host = app.host()?;
        let answer = app.rig.open(&host);
        app.rig.start_writer(host);
        Ok(answer)
    })
    .await
}

/// The page's upgrade of the legacy keys, stored only while no record exists (D12, D13).
#[tauri::command]
pub async fn rig_import(text: String, frontend_epoch: u32, app: tauri::AppHandle) -> Result<RigAnswer, String> {
    blocking("rig_import", move |engine_app| {
        use tauri::Manager;
        let host = engine_app.host()?;
        let answer = engine_app.rig.import(&host, (&app.state::<PluginHostState>(), frontend_epoch), &text);
        engine_app.rig.start_writer(host);
        answer
    })
    .await
}

/// The page showed notice `id` (D13).
#[tauri::command]
pub async fn rig_seen(id: NoticeId) -> Result<(), String> {
    blocking("rig_seen", move |app| {
        app.rig.seen(id);
        Ok(())
    })
    .await
}
