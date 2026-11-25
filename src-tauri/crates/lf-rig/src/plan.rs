//! The restore's plan (D3): what the restore holds for each record unit while it runs. The restore
//! machine (CP2c, `restore.rs`) fills and moves it; the projection (CP2a, `project.rs`) reads it, so
//! a record unit the restore has not finished keeps its place in every write, whatever its plate shows.
//! Pure values: the app's glue keeps one `Plan` in its `Rig` and hands both modules a reference.

use std::collections::BTreeMap;

use lf_engine::{InstallToken, UnitId};

use crate::record::UnitRecord;

/// The restore's plan for both slots.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Plan {
    /// Per slot (A, then B): the record's units in record order at restore start, then the units
    /// placement moved in from `pending` (D9), each at its record place. The projection places a
    /// unit with no plate after the nearest unit before it here that the output holds.
    pub units: [Vec<UnitRecord>; 2],
    /// Per slot: the legacy sends' units still to place, as the record keeps them until placement
    /// moves them into `units` (D9).
    pub pending: [Vec<UnitRecord>; 2],
    /// One disposition per id in `units` (and only those). An id leaves the map, and its unit the
    /// record, only by a confirmed removal (D3): the plate carrying its token leaves the model.
    pub dispositions: BTreeMap<UnitId, Disposition>,
}

/// Where the restore stands with one record unit (D3, D5). `Queued`, `Issued` and `Held` keep the unit
/// at its record place whatever its plate shows; `Restored` and `Matched` hand it to the projection by
/// its token; `Forgotten` and `Yielded` drop it from the record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// Not added yet.
    Queued,
    /// Its add is out. `token` is `None` while the add call has not answered, and the attempt's
    /// token once an answer named it (an [`crate::AddFail::Unresolved`], a `Waiting` [`crate::AddOk`]).
    Issued { token: Option<InstallToken> },
    /// Its add seated, under this token.
    Restored { token: InstallToken },
    /// The model already held it (same slot, id, kind and plugin identity, D9): never added.
    Matched { token: InstallToken },
    /// Not restored this launch, kept at its place for the next: missing from the scan, a host or seat
    /// refusal, an unresolved add whose owner exited, the restore stopped, no plugin host. `why` is for
    /// logs; a retry in this process (D5's `Device`) is the machine's own state.
    Held { why: String },
    /// Dropped: its plugin's own load failed (D5) or a crash marker named it (D6). Its tone files go
    /// with it (the machine's `Forget` action).
    Forgotten,
    /// Dropped: a source the player put in heads the slot (D9, [`crate::AddFail::SourceTaken`]).
    Yielded,
}
