//! The record's file and the writer's rules (D3, D8, D13), over a [`Disk`] the app's glue implements
//! (CP2b: the data folder, `tone::write_atomic`, `File::try_lock`), so every rule here is tested on
//! Linux with a fake that fails on demand.
//!
//! - **One owner per file pair (D8).** [`Store::open`] takes the lock file before its first read;
//!   refused (another process owns the pair) or no data folder, the store is `unavailable`: it reads
//!   nothing, and neither writes the record nor reads, writes or removes the crash marker. The lock
//!   is held as long as the `Disk` lives; a crash releases it with the process.
//! - **Only a missing file is no record (D8).** Text that is not a v1 record, or a read that failed,
//!   is `unreadable`: kept byte for byte, never written over, nothing written that process (the
//!   crash marker neither written nor removed).
//! - **The commit criterion (D3).** A write commits when [`Disk::write_atomic`] returned Ok (written,
//!   synced, renamed); anything else leaves what was stored before, and the state dirty.
//! - **The writer's pace (D3).** The glue projects at each [`TICK`] and hands the result to
//!   [`Store::tick`]: units, order or `pending` changed are written at that tick; values once they have
//!   been quiet for [`SAVE_QUIET`]. A failed write is retried at the next tick (a failed
//!   [`Store::commit`] too: its record is what the store owes until a tick projects otherwise), and a
//!   failure run reports one log line (its first failure); no logging here, the glue logs what it is
//!   told.
//! - **Close (D7).** [`Store::close`] commits the last projection if it differs, then the store writes
//!   no record again (a restore add that answers after close writes nothing).

use std::io;
use std::time::{Duration, Instant};

use crate::answer::RigStatus;
use crate::project::{change, Change};
use crate::record::{RigRecord, Unreadable};

/// The writer's tick: a structure change is stored at the next one (D3).
pub const TICK: Duration = Duration::from_millis(100);

/// How long values must be quiet before the writer stores them: the tone store's quiet
/// (`host/tone.rs` `SAVE_QUIET`), so a fader drag or a knob turn is one write, not one per tick.
pub const SAVE_QUIET: Duration = Duration::from_secs(2);

/// The rig's three files in the app-local data folder, per build kind (Settled 6): `tauri dev` and
/// the release share the folder, so a DEV build keeps its own pair and lock. The glue picks
/// [`RELEASE_FILES`] or [`DEV_FILES`] by `debug_assertions`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Files {
    /// The record.
    pub record: &'static str,
    /// The crash marker (D6).
    pub marker: &'static str,
    /// The lock file one process holds for the pair (D8).
    pub lock: &'static str,
}

/// A release build's files.
pub const RELEASE_FILES: Files = Files { record: "rig.json", marker: "rig-inflight.json", lock: "rig.lock" };

/// A `debug_assertions` build's files.
pub const DEV_FILES: Files = Files { record: "rig.dev.json", marker: "rig-inflight.dev.json", lock: "rig.dev.lock" };

/// The data folder as the store needs it. Names are [`Files`]' (no paths): the implementation joins
/// them to its folder.
pub trait Disk {
    /// The file's bytes, or `Ok(None)` when there is no such file. Any `Err` is a file that exists
    /// and could not be read.
    fn read(&mut self, name: &str) -> io::Result<Option<Vec<u8>>>;
    /// Writes `bytes` beside the file, syncs them and renames them over it. `Ok` only when all three
    /// did; on `Err` the file holds what it held before (a rename that failed included).
    fn write_atomic(&mut self, name: &str, bytes: &[u8]) -> io::Result<()>;
    /// Removes the file; a missing file is `Ok`.
    fn remove(&mut self, name: &str) -> io::Result<()>;
    /// Takes the lock file exclusively without waiting, held until this `Disk` drops. `Err` when
    /// another holder has it or there is no data folder.
    fn try_lock(&mut self, name: &str) -> io::Result<()>;
}

/// What [`Store::open`] found.
#[derive(Clone, Debug, PartialEq)]
pub enum Opened {
    /// The record as read (tolerantly, [`RigRecord::read`]).
    Present(RigRecord),
    /// No record file.
    Absent,
    /// A file that is not a v1 record, or that could not be read: kept, never written over.
    Unreadable(Unreadable),
    /// The lock was refused or there is no data folder; nothing was read. `why` is for the log.
    Unavailable { why: String },
}

impl Opened {
    /// The status the page is answered (D13).
    pub fn status(&self) -> RigStatus {
        match self {
            Opened::Present(_) => RigStatus::Present,
            Opened::Absent => RigStatus::Absent,
            Opened::Unreadable(_) => RigStatus::Unreadable,
            Opened::Unavailable { .. } => RigStatus::Unavailable,
        }
    }
}

/// The answer to a write ([`Store::commit`], [`Store::import`], [`Store::tick`], [`Store::close`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Commit {
    /// Durable: written, synced and renamed, or already what the file holds.
    Committed,
    /// The write failed; what was stored before stands and the state stays dirty. `log` is true for a
    /// failure run's first failure only (the one log line D3 allows per run); a commit ends the run.
    Failed { why: String, log: bool },
    /// Nothing was written, by rule.
    Refused(Refusal),
}

/// Why the store wrote nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The import found a record: it stores nothing and the page is answered `present` (D13).
    Present,
    /// No record yet: only the import stores the first one.
    Absent,
    /// The file is unreadable: never written over (D8).
    Unreadable,
    /// Another process owns the files, or there is no data folder (D8).
    Unavailable,
    /// The rig closed (D7).
    Closed,
}

/// The record's file for one process.
pub struct Store<D: Disk> {
    disk: D,
    files: Files,
    status: RigStatus,
    /// What the file holds, as this store read or last committed it.
    stored: Option<RigRecord>,
    /// The last projection a tick saw and when it last changed, for the values' quiet.
    seen: Option<(RigRecord, Instant)>,
    /// Inside a failure run: a write failed and none has committed since.
    failing: bool,
    closed: bool,
}

impl<D: Disk> Store<D> {
    /// Takes the lock, then reads the record (D8).
    pub fn open(mut disk: D, files: Files) -> (Store<D>, Opened) {
        let opened = match disk.try_lock(files.lock) {
            Err(e) => Opened::Unavailable { why: e.to_string() },
            Ok(()) => match disk.read(files.record) {
                Ok(None) => Opened::Absent,
                Ok(Some(bytes)) => match RigRecord::read(&bytes) {
                    Ok(record) => Opened::Present(record),
                    Err(e) => Opened::Unreadable(e),
                },
                Err(e) => Opened::Unreadable(Unreadable { why: format!("cannot read: {e}") }),
            },
        };
        let stored = match &opened {
            Opened::Present(record) => Some(record.clone()),
            _ => None,
        };
        let store = Store { disk, files, status: opened.status(), stored, seen: None, failing: false, closed: false };
        (store, opened)
    }

    /// What the record is for this process now (an import that committed turns `absent` into
    /// `present`).
    pub fn status(&self) -> RigStatus {
        self.status
    }

    /// The record as the file holds it, when there is one this store may write.
    pub fn stored(&self) -> Option<&RigRecord> {
        self.stored.as_ref()
    }

    /// Whether the last projection a tick saw is not yet stored.
    pub fn dirty(&self) -> bool {
        match (&self.seen, &self.stored) {
            (Some((seen, _)), Some(stored)) => change(stored, seen) != Change::None,
            _ => false,
        }
    }

    /// A durable step's write, now (the restore's commits, D3): `Committed` when the file holds
    /// `record` (in meaning) after it. A failed one leaves `record` as what the store owes, so it is
    /// [`Store::dirty`] and the next [`Store::tick`] retries it.
    pub fn commit(&mut self, record: &RigRecord) -> Commit {
        if let Some(refusal) = self.refusal() {
            return Commit::Refused(refusal);
        }
        let commit = match &self.stored {
            None => return Commit::Refused(Refusal::Absent),
            Some(stored) if change(stored, record) == Change::None => return Commit::Committed,
            Some(_) => self.write(record),
        };
        if matches!(commit, Commit::Failed { .. }) {
            // Owed as the last projection seen; a failure run retries at once, so the instant only
            // matters once the tick projects something else.
            let since = self.seen.as_ref().map_or_else(Instant::now, |(_, since)| *since);
            self.seen = Some((record.clone(), since));
        }
        commit
    }

    /// The page's import (D13), as [`crate::project::import`] made it: stored only while no record
    /// exists. A failed write leaves the store `absent`, so the next launch answers `absent` and the
    /// page imports again.
    pub fn import(&mut self, record: &RigRecord) -> Commit {
        if let Some(refusal) = self.refusal() {
            return Commit::Refused(refusal);
        }
        if self.status == RigStatus::Present {
            return Commit::Refused(Refusal::Present);
        }
        let commit = self.write(record);
        if commit == Commit::Committed {
            self.status = RigStatus::Present;
        }
        commit
    }

    /// The writer's tick (D3): `projected` is this tick's projection. Answers `None` when nothing was
    /// due (clean, or values still settling), else the write's answer.
    pub fn tick(&mut self, projected: RigRecord, now: Instant) -> Option<Commit> {
        if let Some(refusal) = self.refusal() {
            return Some(Commit::Refused(refusal));
        }
        let moved = self.seen.as_ref().is_none_or(|(seen, _)| change(seen, &projected) != Change::None);
        if moved {
            self.seen = Some((projected, now));
        }
        let (seen, since) = self.seen.as_ref()?;
        let due = match change(self.stored.as_ref()?, seen) {
            Change::None => false,
            Change::Structure => true,
            // A failed write is retried at the next tick, whatever it carried.
            Change::Values => self.failing || now.saturating_duration_since(*since) >= SAVE_QUIET,
        };
        if !due {
            return None;
        }
        let record = seen.clone();
        Some(self.write(&record))
    }

    /// Close (D7): commits `last` if it differs from what is stored, then writes no record again.
    pub fn close(&mut self, last: &RigRecord) -> Commit {
        let commit = self.commit(last);
        self.closed = true;
        commit
    }

    /// The crash marker's bytes, `Ok(None)` when there is none. `Err` when this store is unavailable
    /// (D8) or the read failed.
    pub fn read_marker(&mut self) -> io::Result<Option<Vec<u8>>> {
        let name = self.files.marker;
        self.marker_disk()?.read(name)
    }

    /// Writes the crash marker, durably (the restore's `CommitMarker`, D6). `Err` when this store is
    /// unavailable or its record unreadable (D8: nothing is written that process).
    pub fn write_marker(&mut self, bytes: &[u8]) -> io::Result<()> {
        let name = self.files.marker;
        self.marker_writer()?.write_atomic(name, bytes)
    }

    /// Removes the crash marker (D6). `Err` as [`Store::write_marker`].
    pub fn remove_marker(&mut self) -> io::Result<()> {
        let name = self.files.marker;
        self.marker_writer()?.remove(name)
    }

    /// The marker is this process's while it owns the file pair, also after close (D7's last step).
    fn marker_disk(&mut self) -> io::Result<&mut D> {
        if self.status == RigStatus::Unavailable {
            return Err(io::Error::other("the rig's files belong to another process"));
        }
        Ok(&mut self.disk)
    }

    /// The marker's writes: refused also while the record is unreadable (D8).
    fn marker_writer(&mut self) -> io::Result<&mut D> {
        if self.status == RigStatus::Unreadable {
            return Err(io::Error::other("the rig's record is unreadable: nothing is written this process"));
        }
        self.marker_disk()
    }

    fn refusal(&self) -> Option<Refusal> {
        match self.status {
            _ if self.closed => Some(Refusal::Closed),
            RigStatus::Unavailable => Some(Refusal::Unavailable),
            RigStatus::Unreadable => Some(Refusal::Unreadable),
            RigStatus::Present | RigStatus::Absent => None,
        }
    }

    fn write(&mut self, record: &RigRecord) -> Commit {
        let result = serde_json::to_vec(record).map_err(io::Error::other).and_then(|bytes| self.disk.write_atomic(self.files.record, &bytes));
        match result {
            Ok(()) => {
                self.stored = Some(record.clone());
                self.failing = false;
                Commit::Committed
            }
            Err(e) => {
                let log = !self.failing;
                self.failing = true;
                Commit::Failed { why: e.to_string(), log }
            }
        }
    }
}
