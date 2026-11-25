//! The restore's plan (D3): what the restore holds for each record unit while it runs, and the one rule
//! both its readers judge an id by ([`Plan::fate`]). The restore machine (CP2c, `restore.rs`) fills and
//! moves it; the projection (CP2a, `project.rs`) reads it, so a record unit the restore has not finished
//! keeps its place in every write, whatever its plate shows. Pure values: the app's glue keeps one `Plan`
//! in its `Rig` and hands both modules a reference.

use std::collections::{BTreeMap, BTreeSet};

use lf_engine::{InstallToken, UnitId};

use crate::place::{self, Match};
use crate::record::UnitRecord;
use crate::view::{PlateView, RigView};

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
    /// record, only when [`Plan::fate`] says [`Fate::Ended`].
    pub dispositions: BTreeMap<UnitId, Disposition>,
    /// Every install token a view the restore was fed has shown: a token seen and then gone from the
    /// view is a confirmed removal ([`Plan::fate`]). The restore adds each view's tokens as it takes it.
    pub seen: BTreeSet<InstallToken>,
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
    /// logs; a retry in this process (D5's `Device`) is the machine's own state. `token` is the
    /// attempt's when an add got past the mint (a seat refusal, an unresolved add that failed later, a
    /// host refusal after the mint), so the projection tells its failed plate from a later plate under
    /// the same id, and the player's removal of that plate from a `Held` that never had one.
    Held { why: String, token: Option<InstallToken> },
    /// Dropped: its plugin's own load failed (D5) or a crash marker named it (D6). Its tone files go
    /// with it (the machine's `Forget` action).
    Forgotten,
    /// Dropped: a source the player put in heads the slot (D9, [`crate::AddFail::SourceTaken`]).
    Yielded,
}

/// What the plan makes of one of its ids at a view ([`Plan::fate`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fate {
    /// The plan's unit is written at its record place: a unit the restore holds (`Queued`, `Issued`,
    /// `Held`, a `Restored` or `Matched` one whose plate no view has shown yet), or a `pending` unit.
    AtPlace,
    /// Its own plate, in the view, is the unit (`Restored`, `Matched`): recorded from that plate.
    Plate(InstallToken),
    /// Out of the record (`Forgotten`, `Yielded`): a plate under its id is judged by the plate rule alone.
    Dropped,
    /// It leaves the plan at this view: its plate was seen and is gone with no plate left under its id
    /// (a confirmed removal), or the player's unit won its id.
    Ended,
}

impl Plan {
    /// **Each id once, and the player's unit wins it** (D3, D9): what the plan makes of `id` at `view`;
    /// `None` for an id the plan does not hold (neither a unit nor `pending`).
    ///
    /// A disposition binds to its own token: its own plate is the restore's, and any other plate under
    /// its id is judged by the plate rule alone. An applied plate of another token under the id is a
    /// unit the player has: it wins the id, the plan's unit (a `pending` one too, either slot) ends, and
    /// nothing is added under the id. A plate that has not applied ends nothing. A unit with a token
    /// (`Issued`, `Held`, `Restored`, `Matched`) also ends by a confirmed removal: a view showed its plate
    /// and this one does not, and (for a held one) no plate holds its id. A token no view has shown ends
    /// nothing: the plan keeps the unit at its place. A `Queued` unit's id held by an applied plate that
    /// is the unit itself (D9's match) does not end it: the restore matches it. An `Issued` unit whose
    /// add has not answered yet never ends: a plate under its id may be the add's own.
    ///
    /// The restore ([`crate::Restore`]) drops an `Ended` id from the plan; the projection
    /// ([`crate::project()`]) writes the plan's unit for `AtPlace` alone. **The glue calls both with the
    /// same fresh view** (`Model::rig_view`), the restore first, so the two never differ on an id.
    pub fn fate(&self, id: UnitId, view: &RigView) -> Option<Fate> {
        let plates = || view.slots.iter().flatten().filter(move |p| p.id == id);
        let present = |t: InstallToken| view.slots.iter().flatten().any(|p| p.added_by == t);
        let gone = |t: InstallToken| self.seen.contains(&t) && !present(t);
        // The player's unit holds the id: an applied plate that is not `own`'s.
        let won = |own: Option<InstallToken>| plates().any(|p| p.applied && Some(p.added_by) != own);
        let ended = |yes: bool| if yes { Fate::Ended } else { Fate::AtPlace };
        let fate = match self.dispositions.get(&id) {
            None if self.pending.iter().flatten().any(|u| u.id == id) => ended(won(None)),
            None => return None,
            Some(Disposition::Restored { token } | Disposition::Matched { token }) => {
                if present(*token) {
                    Fate::Plate(*token)
                } else {
                    ended(gone(*token))
                }
            }
            Some(Disposition::Issued { token: None }) => Fate::AtPlace,
            Some(Disposition::Queued) => ended(plates().any(|p| p.applied && !self.is_match(id, p, view))),
            Some(Disposition::Held { token: None, .. }) => ended(won(None)),
            Some(Disposition::Issued { token: Some(t) } | Disposition::Held { token: Some(t), .. }) => {
                ended(won(Some(*t)) || (gone(*t) && plates().next().is_none()))
            }
            Some(Disposition::Forgotten | Disposition::Yielded) => Fate::Dropped,
        };
        Some(fate)
    }

    /// Whether `plate` is the record unit `id` itself (D9's match): the restore matches it, not yields.
    fn is_match(&self, id: UnitId, plate: &PlateView, view: &RigView) -> bool {
        (0..2).any(|slot| {
            self.units[slot].iter().find(|u| u.id == id).is_some_and(|u| place::find_match(slot, u, view) == Match::Matched(plate.added_by))
        })
    }

    /// Every id the plan holds: its units' and its `pending`'s, both slots.
    pub fn ids(&self) -> Vec<UnitId> {
        self.units.iter().chain(&self.pending).flatten().map(|u| u.id).collect()
    }
}
