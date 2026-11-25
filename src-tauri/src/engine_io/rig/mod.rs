//! OWNS: chain mode's rig record on disk, one [`Rig`] per process (in `EngineApp`): its files and their
//! lock (D8), the page's handover (`rig_open`, `rig_import`, `rig_seen`; D13), the writer's 100 ms tick
//! and the close at shutdown (D7). The rules are the pure crate `crates/lf-rig` (its crate doc); this is
//! the Windows glue that does the I/O and drives them. `disk` is the data folder ([`lf_rig::Disk`]),
//! `writer` the tick and the close, `restore` the restore's driver and the host it drives
//! ([`RestoreHost`]).
//!
//! **Who writes.** Only this `Rig` writes the record, and only the restore's `CommitMarker` the crash
//! marker; the page writes neither. The record changes in three places: the page's import (once, while
//! no record exists), the rig thread (the writer's tick and the restore's commits, one at a time), and
//! the close, after which nothing is written.
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
//! A `host_init` after that check, while the commit writes, is let through on purpose: any
//! check-then-write keeps that window unless `host_init` took `disk`, and an old page's import is the
//! same upgrade of the same legacy keys, so the new page's `present` answers the right content.
//!
//! **Close** ([`Rig::close`], D7) decides under `disk` whether it writes: the store's status there,
//! not `inner`'s copy, which an import or a first open that committed may not have published yet (close
//! takes the store's record and publishes it). Before its copy it waits, bounded, for two completed
//! feed passes, so an install or a parameter the engine applied and the event ring still holds (or held
//! back for a full ring) reaches the model first; it never drains the feed itself. Once `closing` is
//! set no tick changes the plan (checked under `inner`), so close judges it at its own view, and no
//! open opens or reads the file. A second close (the updater's shutdown, then the exit's) returns only
//! once the first has written and released the lock file, and does nothing.
//!
//! **The plan is the restore's (D3, D10).** The first call that sees a record (an open, the import's
//! commit, or a close that comes first) feeds [`lf_rig::Restore`] `Opened { record, marker }` (the
//! marker as the store's open read it), then the views it has: the import's adoption view first (kept
//! under `disk` with its commit, so a close that sees the record before the import's own `keep` still
//! matches each adopted unit to its plate, and the player's removal of it ends it), then its own. From
//! then on [`lf_rig::Restore::plan`] is the plan every projection writes by, and a later open (a
//! reload) starts nothing (D1). **Restore then project, same view:** each tick feeds the restore the
//! driver's inputs, then `ViewChanged` with a fresh `rig_view`, then projects that view with the plan,
//! all under one `inner`, so the two never judge an id apart ([`lf_rig::Plan::fate`]); a view is taken
//! after the inputs, so no view older than an add's answer is fed after it (its plate would read as
//! removed).
//!
//! **The driver** (`restore.rs`) runs only with a [`RestoreHost`] ([`Rig::with_restore`]); the app's
//! `Rig::new` has none today (the real host is CP3b's: `mode.rs`, where `EngineApp` makes its `Rig`, is
//! where it plugs in). Without one the restore is fed the record and every view but never seeded, so
//! its plan holds every record unit the model lacks at its place (`Queued`), matches a unit the model
//! holds to its plate, and lets an id go to the player's applied unit: the writer keeps the record and
//! adds what the player applies, and no marker is written.
//!
//! **What the plan cannot see:** a record id the restore holds with no plate, which the player's unit
//! takes, applies and leaves between two views, comes back from the plan: no view showed that plate
//! (the player's ids are random `u32`s, so this needs a collision or a page reusing a record id).
//! Closing it needs the model to report a plate that applied and left between two views.
//!
//! **Locks.** `inner` (the restore with its plan and notice, the stored record's copy the projection
//! reads, the restore's actions not yet run) and `disk` (the store, so one write at a time, and
//! `closed`, the done flag a second close waits on). Neither is held across a `settings` or `state`
//! take, a scan, an add or a wait (a second close's wait on `closed_cv` releases `disk`), nor are the
//! two held together; `disk` is held across one `write_atomic` at most (`remove` and a write for the
//! marker's removal). A record write is never made under `state` or `settings`. The driver's own state
//! (`driver`) is the rig thread's: taken before `inner` or `disk`, never by an open, an import or close.
//!
//! **Release.** The `rig_*` commands are DEV-only (`lib.rs`), in lockstep with `lf.chainNative`, so a
//! release build opens no rig and runs no writer before activation; its shutdown still calls
//! [`Rig::close`], which then has nothing to write.

mod disk;
mod restore;
#[cfg(test)]
mod restore_tests;
#[cfg(test)]
mod tests;
mod writer;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::{AcqRel, Acquire, Relaxed}};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use lf_rig::marker::{self, MarkerRead};
use lf_rig::{Action, Commit, Event, Files, ImportError, NoticeId, Opened, Refusal, Restore, RigAnswer, RigRecord, RigStatus, RigView, Store, Volumes, IMPORT_STALE};

use super::mode::{engine, EngineApp};
use super::EngineHost;
use crate::host::PluginHostState;
pub(crate) use disk::files;
use disk::FileDisk;
#[allow(unused_imports)] // the real host (CP3b) implements them
pub(crate) use restore::{Forgot, RestoreHost, SeedTaken};
use restore::Driver;
pub(crate) use writer::RigModel;

/// The process's rig record (the module doc).
pub(crate) struct Rig {
    /// The app-local data folder; `None` without one (every open answers `unavailable`).
    dir: Option<PathBuf>,
    files: Files,
    /// Set first by [`Rig::close`] (D7): no write starts after it. Every write checks it under `disk`.
    closing: AtomicBool,
    /// The writer thread was started (once per process).
    writer: AtomicBool,
    /// What the restore drives (the engine, the chain host); `None`: no restore runs this process.
    host: Option<Arc<dyn RestoreHost>>,
    /// The driver's own state, the rig thread's (`restore.rs`).
    driver: Mutex<Driver>,
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
    /// The restore was fed the first record this process found or imported (`Opened`).
    opened: bool,
    /// The restore (D10): its plan is the projection's, its notice what every open answers (D13).
    restore: Restore,
    /// The restore's actions the driver has not run yet, in order.
    actions: Vec<Action>,
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
    /// The crash marker as the store's open found it (D6), for the restore's `Opened`.
    marker: Option<MarkerRead>,
    /// The view the import adopted the chains at (D12), kept with its commit: the restore's plan is
    /// matched at it first, whoever makes the plan.
    adopted: Option<RigView>,
}

/// What a step under `disk` found, for `inner` and the answer.
struct Seen {
    status: RigStatus,
    stored: Option<RigRecord>,
    at: u64,
    marker: MarkerRead,
    adopted: Option<RigView>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Rig {
    /// The app's rig: no restore runs (its host is CP3b's; it plugs in here, by [`Rig::with_restore`]).
    pub(crate) fn new(dir: Option<PathBuf>, files: Files) -> Rig {
        Rig::build(dir, files, None)
    }

    /// A rig whose restore drives `host` (D10).
    #[allow(dead_code)] // the app's real host is CP3b's; the tests drive a fake one
    pub(crate) fn with_restore(dir: Option<PathBuf>, files: Files, host: Arc<dyn RestoreHost>) -> Rig {
        Rig::build(dir, files, Some(host))
    }

    fn build(dir: Option<PathBuf>, files: Files, host: Option<Arc<dyn RestoreHost>>) -> Rig {
        Rig {
            dir,
            files,
            closing: AtomicBool::new(false),
            writer: AtomicBool::new(false),
            host,
            driver: Mutex::default(),
            inner: Mutex::default(),
            disk: Mutex::default(),
            closed_cv: Condvar::new(),
        }
    }

    /// `rig_open` (D13): what the record is for this process, each slot's volume the fader shows (the
    /// settings' slot gain, else the record's: D2) and the notice. The first call opens the store: the
    /// lock, then the read (D8). The first that finds a record starts the restore and sends its seed
    /// before it answers (D1); a later call answers the same facts and starts nothing.
    pub(crate) fn open(&self, model: &(impl RigModel + ?Sized)) -> RigAnswer {
        let held = model.gains();
        let seen = self.opened();
        let view = model.view();
        let started = self.keep(&seen, Some(&view));
        self.start(started);
        self.answer(&seen, held)
    }

    /// `rig_import` (D12, D13): the page's upgrade of the legacy keys, stored only while no record
    /// exists (else answered `present`, nothing stored). Each slot whose model holds applied units takes
    /// them from a fresh view in place of the import's (DEV adoption); an import id the model holds
    /// refuses it. Its commit is its durability: a failed write is an `Err`, and the next launch answers
    /// `absent`. A replaced page's import is `Err("stale")`, checked at the call and again under `disk`
    /// right before the commit (a `host_init` meanwhile stores nothing).
    pub(crate) fn import(&self, model: &(impl RigModel + ?Sized), doc: (&PluginHostState, u32), text: &str) -> Result<RigAnswer, String> {
        let stale = || doc.0.frontend_epoch.load(Relaxed) != doc.1;
        if stale() {
            return Err(IMPORT_STALE.to_string());
        }
        let seen = self.opened();
        if seen.status != RigStatus::Absent {
            let held = model.gains();
            let started = self.keep(&seen, Some(&model.view()));
            self.start(started);
            return Ok(self.answer(&seen, held));
        }
        self.keep(&seen, None);
        let held = model.gains();
        let view = model.view();
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
                disk.adopted = Some(view.clone());
            }
            (commit, Self::found(&disk))
        };
        // `present` after its commit is an open with that record: the plan is made at the adoption's
        // view (an adopted unit matched to its plate), and the restore starts.
        let started = self.keep(&seen, Some(&view));
        self.start(started);
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
        let actions = lock(&self.inner).restore.handle(Event::Seen(id));
        debug_assert!(actions.is_empty(), "`Seen` answers no action");
    }

    /// The store, opened by the first call (the lock, then the read), as it stands. Once `closing` is
    /// set nothing is opened or read: an open that comes first then answers `unavailable` (the status
    /// with no store, as after close: this process writes no record, and the page imports nothing).
    fn opened(&self) -> Seen {
        let mut disk = lock(&self.disk);
        if disk.store.is_none() && !disk.closed && !self.closing.load(Acquire) {
            let (mut store, opened) = Store::open(FileDisk::new(self.dir.clone()), self.files);
            match &opened {
                Opened::Unavailable { why } => log::warn!("[rig] the rig record is unavailable this launch: {why}"),
                Opened::Unreadable(u) => log::warn!("[rig] the rig record is unreadable; it is kept and nothing is written this launch: {}", u.why),
                Opened::Present(_) | Opened::Absent => {}
            }
            // The marker as this launch found it (D6), before the restore writes one. One that cannot
            // be read counts as unreadable: no plugin is restored this launch.
            if matches!(opened, Opened::Present(_) | Opened::Absent) {
                disk.marker = Some(match store.read_marker() {
                    Ok(bytes) => marker::read(bytes.as_deref()),
                    Err(e) => {
                        log::warn!("[rig] the crash marker could not be read ({e})");
                        MarkerRead::Unreadable
                    }
                });
            }
            disk.store = Some(store);
            disk.at += 1;
        }
        Self::found(&disk)
    }

    fn found(disk: &DiskState) -> Seen {
        let (status, stored) = match &disk.store {
            Some(store) => (store.status(), store.stored().cloned()),
            None => (RigStatus::Unavailable, None),
        };
        let marker = disk.marker.clone().unwrap_or(MarkerRead::None);
        Seen { status, stored, at: disk.at, marker, adopted: disk.adopted.clone() }
    }

    /// Keep the store's copy in `inner` unless a newer one is there. The first record the process finds
    /// or imports starts the restore (`Opened`), then is judged at the adoption's view (an import's) and
    /// at `view` (taken after the store's copy, each lock released), in that order; the restore's
    /// actions so far are answered for the caller to start ([`Rig::start`]; close drops them). Every
    /// call that can first see a record passes its view; a tick's (after a commit) need not.
    fn keep(&self, seen: &Seen, view: Option<&RigView>) -> Vec<Action> {
        let mut inner = lock(&self.inner);
        let inner = &mut *inner;
        if seen.at >= inner.stored_at {
            inner.stored = seen.stored.clone();
            inner.stored_at = seen.at;
        }
        let mut started = Vec::new();
        if !inner.opened {
            if let (Some(record), Some(view)) = (&seen.stored, view) {
                inner.opened = true;
                started.extend(inner.restore.handle(Event::Opened { record: record.clone(), marker: seen.marker.clone() }));
                for view in seen.adopted.iter().chain([view]) {
                    started.extend(inner.restore.handle(Event::ViewChanged(view.clone())));
                }
            }
        }
        started
    }

    fn answer(&self, seen: &Seen, held: Volumes) -> RigAnswer {
        let volumes = match (&seen.status, &seen.stored) {
            (RigStatus::Present, Some(record)) => Some(lf_rig::volumes(record, held)),
            _ => None,
        };
        RigAnswer { status: seen.status, volumes, notice: lock(&self.inner).restore.notice().cloned() }
    }

    /// Start the writer's tick on the rig thread, once per process (the first `rig_open` or
    /// `rig_import`). With a restore host the tick reads the model through it.
    pub(crate) fn start_writer(self: &Arc<Self>, host: EngineHost) {
        if self.writer.swap(true, AcqRel) {
            return;
        }
        let rig = self.clone();
        let spawned = std::thread::Builder::new().name("lf-rig".into()).spawn(move || match rig.host.clone() {
            Some(restore) => writer::run(&rig, &*restore),
            None => writer::run(&rig, &host),
        });
        if let Err(e) = spawned {
            log::error!("[rig] no thread for the rig record's writer ({e}); changes are stored at close only");
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
