//! The crash marker (D6): `rig-inflight.json` (`rig-inflight.dev.json` in a DEV build), the record of
//! the plugin loads the restore issued that are not yet known to be safe. A process that dies inside
//! a plugin's load leaves it on disk, and the next process's first open forgets the units it names
//! instead of loading them into a second crash.
//!
//! - **One entry per issued plugin load, held until terminal.** A load is terminal when its plate
//!   applied and then lived [`SETTLE`] on, or when it failed with its owner gone (its exit latch set).
//!   `Waiting`, an unresolved add and a setup timeout whose owner may still be in plugin code are not.
//!   A terminal load leaves the marker by a durable rewrite; a host refusal (no plugin code ran) leaves
//!   it at the next write, which comes before the next load ([`Loads`]).
//! - **The file** is `{ "version": 1, "units": [id, …] }`, written with the record's durability (synced,
//!   renamed). One naming nothing reads as none ([`read`]): the restore writes it before its first add
//!   of any kind, so a crash in a built-in's add forgets nothing.
//! - **Unreadable** (bad text, another version, an id 0): no plugin is restored that launch, the record
//!   is left as it is, and the marker goes ([`MarkerRead::Unreadable`]).
//! - **Removal** unlinks the file; a failed unlink writes an empty marker in its place ([`clear`]).
//! - **At a clean close** (SEAT-C) a load inside its settle counts as terminal: the process lived
//!   through it ([`Loads::at_close`]).
//!
//! The rules only: the app's glue reads and writes the file (`engine_io/rig/`), and the restore
//! machine ([`crate::restore`]) decides what it names.

use std::collections::BTreeMap;
use std::time::Duration;

use lf_engine::{InstallToken, UnitId};
use serde::{Deserialize, Serialize};

/// How long a plugin load that applied must run before it leaves the marker (D6).
pub const SETTLE: Duration = Duration::from_secs(3);

/// The marker's file version.
const VERSION: u32 = 1;

/// The marker as stored.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MarkerFile {
    version: u32,
    units: Vec<UnitId>,
}

/// What a process found on disk at its first open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarkerRead {
    /// No marker, or one naming nothing.
    None,
    /// The units whose loads the last process issued and never saw end safely.
    Names(Vec<UnitId>),
    /// A file that cannot be read: no plugin is restored this launch, and the marker goes.
    Unreadable,
}

/// The marker's text as found: `None` when the file is missing.
pub fn read(text: Option<&[u8]>) -> MarkerRead {
    let Some(text) = text else { return MarkerRead::None };
    match serde_json::from_slice::<MarkerFile>(text) {
        Ok(file) if file.version == VERSION && !file.units.contains(&0) => {
            if file.units.is_empty() {
                MarkerRead::None
            } else {
                MarkerRead::Names(file.units)
            }
        }
        _ => MarkerRead::Unreadable,
    }
}

/// The marker's text naming `ids`.
pub fn text(ids: &[UnitId]) -> Vec<u8> {
    serde_json::to_vec(&MarkerFile { version: VERSION, units: ids.to_vec() }).unwrap_or_default()
}

/// How a removal went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cleared {
    /// The file is gone.
    Removed,
    /// The unlink failed and an empty marker (read as none) stands in its place.
    Emptied,
    /// Both failed: the marker still names what it named, and the next launch forgets those units (a
    /// false crash, told then). One log line.
    Failed,
}

/// Remove the marker: `unlink` it, and when that fails, `write` the empty marker ([`text`] of none)
/// over it with the commit's durability.
pub fn clear<E>(unlink: impl FnOnce() -> Result<(), E>, write: impl FnOnce(&[u8]) -> Result<(), E>) -> Cleared {
    if unlink().is_ok() {
        Cleared::Removed
    } else if write(&text(&[])).is_ok() {
        Cleared::Emptied
    } else {
        Cleared::Failed
    }
}

/// The plugin loads the marker follows, by unit id: issued and not yet terminal.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Loads {
    loads: BTreeMap<UnitId, Load>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Load {
    /// The attempt's install token, once an answer named it.
    token: Option<InstallToken>,
    /// Its plate applied: the settle runs.
    applied: bool,
    /// It failed (its plate failed, or left the model, before it applied).
    failed: bool,
    /// Its owner thread ended (the exit latch).
    exited: bool,
}

impl Loads {
    /// The ids the marker must name now: each load issued and not terminal, in id order.
    pub fn named(&self) -> Vec<UnitId> {
        self.loads.keys().copied().collect()
    }

    /// What the marker keeps at a clean close (SEAT-C): a load inside its settle goes too.
    pub fn at_close(&self) -> Vec<UnitId> {
        self.loads.iter().filter(|(_, l)| !l.applied).map(|(id, _)| *id).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.loads.is_empty()
    }

    pub fn contains(&self, id: UnitId) -> bool {
        self.loads.contains_key(&id)
    }

    /// `id`'s load is about to be issued: the next marker write names it, before the load.
    pub fn issue(&mut self, id: UnitId) {
        self.loads.insert(id, Load::default());
    }

    /// `id`'s load never ran plugin code (a host refusal, a match found after its write, a write that
    /// failed): it leaves the marker at the next write, which no rewrite of its own is owed for.
    pub fn withdraw(&mut self, id: UnitId) {
        self.loads.remove(&id);
    }

    /// An answer named `id`'s attempt.
    pub fn answered(&mut self, id: UnitId, token: InstallToken) {
        if let Some(load) = self.loads.get_mut(&id) {
            load.token = Some(token);
        }
    }

    /// The attempt `token` applied: true when this starts its settle. A failure seen before (a plate
    /// that read failed, then applied) no longer counts: only the settle ends the load now, so a latch
    /// that comes later does not cut it short.
    pub fn applied(&mut self, token: InstallToken) -> bool {
        self.by_token(token).is_some_and(|load| {
            load.failed = false;
            !std::mem::replace(&mut load.applied, true)
        })
    }

    /// The attempt tokens answers named, of the loads still followed.
    pub fn tokens(&self) -> Vec<InstallToken> {
        self.loads.values().filter_map(|l| l.token).collect()
    }

    /// The settle of `token` ran out: true when its load became terminal (a rewrite is owed).
    pub fn settled(&mut self, token: InstallToken) -> bool {
        self.end_if(token, |load| load.applied)
    }

    /// The attempt `token` failed (its plate failed or left before it applied; `exited` when its owner
    /// had ended by then): true when its load became terminal.
    pub fn failed(&mut self, token: InstallToken, exited: bool) -> bool {
        if let Some(load) = self.by_token(token) {
            load.failed |= !load.applied;
            load.exited |= exited;
        }
        self.end_if(token, |load| load.failed && load.exited)
    }

    /// The owner of `token` ended (its exit latch): true when its load became terminal.
    pub fn exited(&mut self, token: InstallToken) -> bool {
        if let Some(load) = self.by_token(token) {
            load.exited = true;
        }
        self.end_if(token, |load| load.failed && load.exited)
    }

    fn by_token(&mut self, token: InstallToken) -> Option<&mut Load> {
        self.loads.values_mut().find(|l| l.token == Some(token))
    }

    fn end_if(&mut self, token: InstallToken, terminal: impl Fn(&Load) -> bool) -> bool {
        let id = self.loads.iter().find(|(_, l)| l.token == Some(token) && terminal(l)).map(|(id, _)| *id);
        id.is_some_and(|id| self.loads.remove(&id).is_some())
    }
}
