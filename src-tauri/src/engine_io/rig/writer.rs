//! The writer (D3) and the close (D7): each [`lf_rig::TICK`] on the rig thread, and once more at
//! shutdown, a fresh copy of the slot gains and the chains, fed to the restore and then projected with
//! its plan, handed to the store. The store decides what is due ([`lf_rig::Store::tick`]: a structure
//! change at once, values after [`lf_rig::SAVE_QUIET`], a failed write again at the next tick); the
//! tick runs every time, whether or not the chain moved, so a failed write is retried and a value
//! change is stored once quiet while nothing else happens. A tick is also the restore driver's turn
//! (`restore.rs`): its inputs before the copy, its actions after the write. The close's order: the
//! module doc of `rig` and [`Rig::close`].

use std::cell::RefCell;
use std::sync::atomic::Ordering::{AcqRel, Acquire};
use std::time::{Duration, Instant};

use lf_engine::Command;
use lf_rig::marker::{self, Cleared};
use lf_rig::{Action, Commit, Event, PlateState, RigRecord, RigStatus, RigView, Store, Volumes};

use super::disk::FileDisk;
use super::{lock, Rig};
use crate::engine_io::chain::STASH_AFTER;
use crate::engine_io::mode::SHUTDOWN_WAIT;
use crate::engine_io::EngineHost;

/// How many completed feed passes close waits for: two, so an event the engine held back for a full
/// event ring is taken too. The first pass frees the ring; the engine publishes what it held back at
/// its next callback (`publish_if_idle` at the feed's next tick when no device runs), and the second
/// pass takes it. A device block longer than a feed frame can leave it to a later pass, past what close
/// waits for.
const FEED_PASSES: u64 = 2;

/// How long close waits for its feed passes, from before its first read of the count: the time after
/// which the model already calls the feed stopped and drains the ring itself (`chain::STASH_AFTER`).
/// The feed drains every frame (~17 ms), so a running feed answers within two or three frames; a feed
/// that is held or gone costs this once, at exit, and the close goes on with the model as it is.
const FEED_PASS_WAIT: Duration = STASH_AFTER;

/// What the rig reads of the chain model and the settings (D2, D3), and close's wait for the feed. The
/// app's [`EngineHost`] is one; the restore's host (`restore::RestoreHost`) is one too, so a test's
/// fake host is the model the writer and the restore read.
pub(crate) trait RigModel {
    /// Each slot's gain as the settings memory holds it (D2), taken under `settings` and released.
    fn gains(&self) -> Volumes;
    /// A fresh copy of the chains ([`lf_rig::RigView`]), taken under `state` and released.
    fn view(&self) -> RigView;
    /// Close's wait, bounded, for the feed to bring what the engine applied into the model (D7).
    fn wait_feed(&self) {}
}

impl RigModel for EngineHost {
    fn gains(&self) -> Volumes {
        slot_gains(self)
    }

    fn view(&self) -> RigView {
        view(self)
    }

    fn wait_feed(&self) {
        wait_feed_passes(self);
    }
}

/// Each slot's gain as the settings memory holds it (D2): what a gesture or the rig's seed last set in
/// this process, `None` for a slot it holds none for. Taken under `settings` and released.
pub(super) fn slot_gains(host: &EngineHost) -> Volumes {
    let mut held = [None; 2];
    for command in host.settings() {
        if let Command::SetSlotGain(slot, gain) = command {
            if let Some(h) = held.get_mut(usize::from(slot)) {
                *h = Some(f64::from(gain));
            }
        }
    }
    held
}

/// The settings' gains as the record spells them: the engine keeps a gain as an `f32`, so a gain that
/// is the record's volume at that precision keeps the record's spelling (no write for a widened 0.8).
fn spelled(held: Volumes, record: &RigRecord) -> Volumes {
    std::array::from_fn(|s| held[s].map(|g| if record.slots[s].volume as f32 == g as f32 { record.slots[s].volume } else { g }))
}

/// The chains as the rig reads them, copied under `state` and released.
pub(super) fn view(host: &EngineHost) -> RigView {
    lock(&host.core.state).model.rig_view()
}

/// The writer's loop on the rig thread: a tick every [`lf_rig::TICK`] (sooner when one of the restore's
/// timers is due) until the rig closes, or its record turns out to be one this process never writes.
pub(super) fn run(rig: &Rig, model: &(impl RigModel + ?Sized)) {
    loop {
        let wait = rig.next_due(Instant::now()).map_or(lf_rig::TICK, |due| due.min(lf_rig::TICK));
        std::thread::sleep(wait);
        if !rig.tick(model, Instant::now()) {
            break;
        }
    }
}

impl Rig {
    /// One tick (D3), the restore driver's turn: the restore's inputs (`Rig::inputs`), then a fresh
    /// copy fed to the restore and projected with its plan, handed to the store; then the restore's
    /// actions (`Rig::drive`). False once nothing will ever be written again (closing, or a record this
    /// process never writes: unreadable, unavailable).
    pub(super) fn tick(&self, model: &(impl RigModel + ?Sized), now: Instant) -> bool {
        match self.writable() {
            Some(true) => {}
            Some(false) => return true, // no record yet: the import makes the first
            None => return false,
        }
        let events = self.inputs(now);
        // The copy is taken after the inputs: an add's answer is never fed after a view older than it.
        let held = model.gains();
        let view = model.view();
        // `None` with no record yet, or once close began (the plan is close's from then on).
        let Some(projected) = self.judge(events, held, view, false) else { return !self.closing.load(Acquire) };
        let (commit, seen) = {
            let mut disk = lock(&self.disk);
            if self.closing.load(Acquire) || disk.closed {
                return false;
            }
            let Some(store) = disk.store.as_mut() else { return true };
            let commit = store.tick(projected, now);
            if commit == Some(Commit::Committed) {
                disk.at += 1;
            }
            (commit, Rig::found(&disk))
        };
        match commit {
            Some(Commit::Committed) => {
                self.keep(&seen, None);
            }
            Some(Commit::Failed { why, log: true }) => log::warn!("[rig] the rig record was not written ({why}); trying again at each tick"),
            _ => {}
        }
        self.drive(model, now);
        true
    }

    /// Close (D7), on the shutdown's thread before `run_shutdown`, once:
    /// 1. set `closing`: no write and no open starts after it; nothing here takes a slot's order lock or
    ///    waits for an add. A second close (the updater's shutdown, then the exit's) waits until the
    ///    first has finished, its write and the lock file's release, bounded by `SHUTDOWN_WAIT`, then
    ///    does nothing;
    /// 2. under `disk`, whether there is a record to write: the store's status and record, not
    ///    `inner`'s copy (an import or a first open may have committed and not yet published it), which
    ///    is final now (every commit but this one checks `closing` under `disk`);
    /// 3. wait, bounded ([`FEED_PASS_WAIT`]), for [`FEED_PASSES`] completed feed passes, so an install
    ///    or a parameter the engine applied and the event ring still holds (or the engine held back for a
    ///    full ring) is in the model; the feed thread drains, never this one;
    /// 4. a fresh copy (the slot gains, then the chains), the store's record published to `inner`, the
    ///    plan judged at the full view, then projected with a `Leaving` plate dropped (the shutdown
    ///    retires its tone). No tick changes the plan after step 1, so close's view is the last one
    ///    that judges it;
    /// 5. commit it if it differs from what is stored;
    /// 6. the crash marker (D6, SEAT-C: `Loads::at_close`), as the restore answers `Closing` after that
    ///    view: a load whose add was issued and has not applied stays named (the next launch forgets
    ///    it); a load that applied goes, also inside its settle, and one never issued goes. A restore
    ///    that never wrote a marker answers nothing, so a marker an earlier launch left stays for the
    ///    next launch's open. Close never waits for the driver: an add in flight answers into a
    ///    restore that is closing, and every write the driver tries after step 1 is refused under
    ///    `disk`;
    /// 7. release the lock file and set `closed`.
    pub(crate) fn close(&self, model: &(impl RigModel + ?Sized)) {
        if self.closing.swap(true, AcqRel) {
            self.wait_closed();
            return;
        }
        let found = Rig::found(&lock(&self.disk));
        let last = if found.stored.is_some() {
            model.wait_feed();
            let held = model.gains();
            let view = model.view();
            // A first sight of the record (no open published it) makes the plan; its seed is not sent.
            let _ = self.keep(&found, Some(&view));
            self.project(held, view, true)
        } else {
            None
        };
        let marker = lock(&self.inner).restore.handle(Event::Closing);
        let mut disk = lock(&self.disk);
        if let Some(mut store) = disk.store.take() {
            if let Some(last) = last {
                match store.close(&last) {
                    Commit::Failed { why, .. } => log::warn!("[rig] the rig record's last write failed at close ({why}); the file keeps the last one stored"),
                    Commit::Committed | Commit::Refused(_) => {}
                }
            }
            close_marker(&mut store, marker);
            // Dropping the store releases the lock file (D8), before `closed` says close is done.
            drop(store);
        }
        disk.closed = true;
        self.closed_cv.notify_all();
    }

    /// A second close: wait (`disk` released while waiting) until the first has set `closed`.
    fn wait_closed(&self) {
        let disk = lock(&self.disk);
        let (_disk, waited) = self.closed_cv.wait_timeout_while(disk, SHUTDOWN_WAIT, |d| !d.closed).unwrap_or_else(|e| e.into_inner());
        if waited.timed_out() {
            log::warn!("[rig] a second close gave up waiting for the first after {} s", SHUTDOWN_WAIT.as_secs());
        }
    }

    /// Whether a tick has anything to write: `Some(true)` with a record, `Some(false)` while the store
    /// waits for the import (or is not open yet), `None` when this process never writes the record.
    fn writable(&self) -> Option<bool> {
        if lock(&self.inner).stored.is_some() {
            return Some(true);
        }
        let disk = lock(&self.disk);
        match disk.store.as_ref().map(|s| s.status()) {
            _ if disk.closed => None,
            None | Some(RigStatus::Absent | RigStatus::Present) => Some(false),
            Some(RigStatus::Unreadable | RigStatus::Unavailable) => None,
        }
    }

    /// The record the chains stand for now, as [`Rig::judge`] makes it with no other event first.
    pub(super) fn project(&self, held: Volumes, view: RigView, at_close: bool) -> Option<RigRecord> {
        self.judge(Vec::new(), held, view, at_close)
    }

    /// The restore first, then the projection, at one view: under `inner`, `events` (the driver's
    /// inputs, in order) and then `ViewChanged(view)` go to the restore, whose actions are queued for
    /// the driver, and its plan (each id the view ends dropped) projects `view` with `held` (the slot
    /// gains, then the view, as the caller took them, each lock released before the next). At close
    /// (`at_close`) a `Leaving` plate is then dropped (the shutdown retires its tone), after the plan
    /// judged the full view. `None` while there is no record, and for a tick once `closing` is set:
    /// checked under `inner`, so a tick changes the plan no more once close began, and close judges it
    /// at its own view (a tick that judged it before used a view older than close's).
    pub(super) fn judge(&self, events: Vec<Event>, held: Volumes, mut view: RigView, at_close: bool) -> Option<RigRecord> {
        let mut inner = lock(&self.inner);
        let inner = &mut *inner;
        if !at_close && self.closing.load(Acquire) {
            return None;
        }
        inner.stored.as_ref()?;
        for event in events.into_iter().chain([Event::ViewChanged(view.clone())]) {
            let actions = inner.restore.handle(event);
            inner.actions.extend(actions);
        }
        if at_close {
            for plates in &mut view.slots {
                plates.retain(|p| p.state != PlateState::Leaving);
            }
        }
        let stored = inner.stored.as_ref()?;
        Some(lf_rig::project(stored, inner.restore.plan(), &view, spelled(held, stored)))
    }
}

/// Close's marker step: the restore's answer to `Closing`, run on the store before it drops.
fn close_marker(store: &mut Store<FileDisk>, actions: Vec<Action>) {
    for action in actions {
        match action {
            Action::CommitMarker(ids) => {
                if let Err(e) = store.write_marker(&marker::text(&ids)) {
                    log::warn!("[rig] the crash marker was not written at close ({e}); the next launch forgets what it named before");
                }
            }
            Action::RemoveMarker => {
                if clear_marker(store) == Cleared::Failed {
                    log::warn!("[rig] the crash marker was not removed at close; the next launch forgets the units it names");
                }
            }
            Action::Log(line) => log::info!("{line}"),
            _ => {}
        }
    }
}

/// [`marker::clear`] on the store: the unlink, else the empty marker in its place.
pub(super) fn clear_marker(store: &mut Store<FileDisk>) -> Cleared {
    let store = RefCell::new(store);
    marker::clear(|| store.borrow_mut().remove_marker(), |bytes| store.borrow_mut().write_marker(bytes))
}

/// Wait until the feed completes [`FEED_PASSES`] passes that began after this call, [`FEED_PASS_WAIT`]
/// at most, its deadline taken before the count's first read (which waits for `settings`). The count is
/// read under `settings`, and polled lock-free after it.
fn wait_feed_passes(host: &EngineHost) {
    let deadline = Instant::now() + FEED_PASS_WAIT;
    let since = host.feed_passes();
    while host.core.feed_passes.load(Acquire).wrapping_sub(since) < FEED_PASSES {
        if Instant::now() >= deadline {
            log::info!("[rig] no {FEED_PASSES} feed passes within {} ms; the close takes the chains as the model holds them", FEED_PASS_WAIT.as_millis());
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}
