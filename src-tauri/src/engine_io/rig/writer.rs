//! The writer (D3) and the close (D7): each [`lf_rig::TICK`] on the rig thread, and once more at
//! shutdown, a fresh copy of the slot gains and the chains, projected with the plan, handed to the
//! store. The store decides what is due ([`lf_rig::Store::tick`]: a structure change at once, values
//! after [`lf_rig::SAVE_QUIET`], a failed write again at the next tick); the tick runs every time,
//! whether or not the chain moved, so a failed write is retried and a value change is stored once
//! quiet while nothing else happens.

use std::sync::atomic::Ordering::{AcqRel, Acquire};
use std::time::Instant;

use lf_engine::Command;
use lf_rig::{Commit, PlateState, RigRecord, RigStatus, RigView, Volumes};

use super::{end_ids, lock, Rig};
use crate::engine_io::EngineHost;

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
        let Some(projected) = self.project(host, false) else { return true };
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
            Some(Commit::Committed) => self.keep(&seen),
            Some(Commit::Failed { why, log: true }) => log::warn!("[rig] the rig record was not written ({why}); trying again at each tick"),
            _ => {}
        }
        true
    }

    /// Close (D7), on the shutdown's thread before `run_shutdown`: set `closing` (no write starts after
    /// it; nothing here takes a slot's order lock or waits for an add), take a fresh copy (the slot gains,
    /// then the chains, a `Leaving` plate dropped: the shutdown retires its tone), project it with the
    /// plan, commit it if it differs from what is stored, then release the lock file. Once.
    ///
    /// The marker is close's last step (D6, SEAT-C): this launch names no load in it before CP3a's
    /// restore issues one, so a marker an earlier launch left stays for the next launch's open to judge.
    pub(crate) fn close(&self, host: &EngineHost) {
        if self.closing.swap(true, AcqRel) {
            return;
        }
        let last = match self.writable() {
            Some(true) => self.project(host, true),
            _ => None,
        };
        let mut disk = lock(&self.disk);
        disk.closed = true;
        // Dropping the store at the end of this scope releases the lock file (D8).
        let Some(mut store) = disk.store.take() else { return };
        if let Some(last) = last {
            match store.close(&last) {
                Commit::Failed { why, .. } => log::warn!("[rig] the rig record's last write failed at close ({why}); the file keeps the last one stored"),
                Commit::Committed | Commit::Refused(_) => {}
            }
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

    /// The record the chains stand for now: the slot gains, then the view (each lock released before
    /// the next), projected under `inner` with the plan. `None` while there is no record.
    fn project(&self, host: &EngineHost, closing: bool) -> Option<RigRecord> {
        let held = slot_gains(host);
        let mut view = view(host);
        if closing {
            for plates in &mut view.slots {
                plates.retain(|p| p.state != PlateState::Leaving);
            }
        }
        let mut inner = lock(&self.inner);
        let inner = &mut *inner;
        let stored = inner.stored.as_ref()?;
        end_ids(&mut inner.plan, &view);
        Some(lf_rig::project(stored, &inner.plan, &view, spelled(held, stored)))
    }
}
