//! The writer (D3) and the close (D7): each [`lf_rig::TICK`] on the rig thread, and once more at
//! shutdown, a fresh copy of the slot gains and the chains, projected with the plan, handed to the
//! store. The store decides what is due ([`lf_rig::Store::tick`]: a structure change at once, values
//! after [`lf_rig::SAVE_QUIET`], a failed write again at the next tick); the tick runs every time,
//! whether or not the chain moved, so a failed write is retried and a value change is stored once
//! quiet while nothing else happens. The close's order: the module doc of `rig` and [`Rig::close`].

use std::sync::atomic::Ordering::{AcqRel, Acquire};
use std::time::{Duration, Instant};

use lf_engine::Command;
use lf_rig::{Commit, PlateState, RigRecord, RigStatus, RigView, Volumes};

use super::{end_ids, lock, Rig};
use crate::engine_io::chain::STASH_AFTER;
use crate::engine_io::mode::SHUTDOWN_WAIT;
use crate::engine_io::EngineHost;

/// How long close waits for one completed feed pass: the time after which the model already calls the
/// feed stopped and drains the ring itself (`chain::STASH_AFTER`). The feed drains every frame
/// (~17 ms), so a running feed answers within a frame or two; a feed that is held or gone costs this
/// once, at exit, and the close goes on with the model as it is.
const FEED_PASS_WAIT: Duration = STASH_AFTER;

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

/// The writer's loop on the rig thread: a tick every [`lf_rig::TICK`] until the rig closes, or its
/// record turns out to be one this process never writes.
pub(super) fn run(rig: &Rig, host: &EngineHost) {
    loop {
        std::thread::sleep(lf_rig::TICK);
        if !rig.tick(host, Instant::now()) {
            break;
        }
    }
}

impl Rig {
    /// One tick (D3): project a fresh copy and hand it to the store. False once nothing will ever be
    /// written again (closing, or a record this process never writes: unreadable, unavailable).
    pub(super) fn tick(&self, host: &EngineHost, now: Instant) -> bool {
        match self.writable() {
            Some(true) => {}
            Some(false) => return true, // no record yet: the import makes the first
            None => return false,
        }
        let held = slot_gains(host);
        let Some(projected) = self.project(held, view(host), false) else { return true };
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
            Some(Commit::Committed) => self.keep(&seen, None),
            Some(Commit::Failed { why, log: true }) => log::warn!("[rig] the rig record was not written ({why}); trying again at each tick"),
            _ => {}
        }
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
    /// 3. wait, bounded ([`FEED_PASS_WAIT`]), for one completed feed pass, so an install or a parameter
    ///    the engine applied and the event ring still holds is in the model; the feed thread drains,
    ///    never this one;
    /// 4. a fresh copy (the slot gains, then the chains), the store's record published to `inner`, the
    ///    plan judged at the full view, then projected with a `Leaving` plate dropped (the shutdown
    ///    retires its tone);
    /// 5. commit it if it differs from what is stored, then release the lock file and set `closed`.
    ///
    /// The marker is close's last step (D6, SEAT-C): this launch names no load in it before CP3a's
    /// restore issues one, so a marker an earlier launch left stays for the next launch's open to judge.
    pub(crate) fn close(&self, host: &EngineHost) {
        if self.closing.swap(true, AcqRel) {
            self.wait_closed();
            return;
        }
        let found = Rig::found(&lock(&self.disk));
        let last = if found.stored.is_some() {
            wait_feed_pass(host);
            let held = slot_gains(host);
            let view = view(host);
            self.keep(&found, Some(&view));
            self.project(held, view, true)
        } else {
            None
        };
        let mut disk = lock(&self.disk);
        if let Some(mut store) = disk.store.take() {
            if let Some(last) = last {
                match store.close(&last) {
                    Commit::Failed { why, .. } => log::warn!("[rig] the rig record's last write failed at close ({why}); the file keeps the last one stored"),
                    Commit::Committed | Commit::Refused(_) => {}
                }
            }
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

    /// The record the chains stand for now: `held` and `view` as the caller took them (the slot gains,
    /// then the view, each lock released before the next), projected under `inner` with the plan, whose
    /// ended ids go first, judged at the full view. At close a `Leaving` plate is then dropped (the
    /// shutdown retires its tone). `None` while there is no record.
    fn project(&self, held: Volumes, mut view: RigView, closing: bool) -> Option<RigRecord> {
        let mut inner = lock(&self.inner);
        let inner = &mut *inner;
        let stored = inner.stored.as_ref()?;
        end_ids(&mut inner.plan, &view);
        if closing {
            for plates in &mut view.slots {
                plates.retain(|p| p.state != PlateState::Leaving);
            }
        }
        Some(lf_rig::project(stored, &inner.plan, &view, spelled(held, stored)))
    }
}

/// Wait until the feed completes one pass that began after this call ([`FEED_PASS_WAIT`] at most). The
/// count is read under `settings`, and polled lock-free after it.
fn wait_feed_pass(host: &EngineHost) {
    let since = host.feed_passes();
    let deadline = Instant::now() + FEED_PASS_WAIT;
    while host.core.feed_passes.load(Acquire) == since {
        if Instant::now() >= deadline {
            log::info!("[rig] no feed pass within {} ms; the close takes the chains as the model holds them", FEED_PASS_WAIT.as_millis());
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}
