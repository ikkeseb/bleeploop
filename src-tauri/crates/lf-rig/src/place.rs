//! Placement (D9): where a restore add goes, which plate already is the unit, and when a slot's
//! `pending` (the legacy sends still to place, unit contract §6) moves into its chain. Pure reads of
//! the plan and the model's view; the restore machine ([`crate::restore`]) acts on the answers.
//!
//! - **The role** of a slot comes from its head, the first unit the record still keeps: a built-in by
//!   kind at once (SYNTH plays notes, the rest are input-fed), a plugin from its **applied** plate's
//!   `source` (a plugin's activation says what it is; the scan does not). A head still to add, loading
//!   or `Waiting` waits and is read again at the next view; one `Forgotten`, yielded or removed lets the
//!   next unit decide; one `Held` keeps `pending` for the next launch. A slot with no record unit reads
//!   the first plate the player put in, and an empty chain is input-fed.
//! - **What the role takes** from `pending`: an input-fed chain all of it, in its order (the OTT, RING
//!   MOD, ECHO, REVERB, as the upgrade wrote it); a source-headed chain the OTT alone, and only when it
//!   is on. One commit moves it to the units' end and clears `pending`, before any of its adds.
//! - **The anchor** of a record unit is the record units before it ([`Place::After`]), resolved by the
//!   app under its slot lock to the nearest one present; a placed unit goes to the chain's end.
//! - **A match** is a live plate of the unit's slot with its id, kind and plugin identity, neither a
//!   failed add nor `Leaving`: the model already holds the unit, so it is never added. Any other live
//!   plate holding the id refuses the add.

use std::collections::BTreeMap;

use lf_engine::{InstallToken, UnitId, UnitKind};

use crate::add::Place;
use crate::plan::Disposition;
use crate::record::{UnitBody, UnitRecord};
use crate::view::{PlateState, PlateView, RigView};

/// What a slot's chain is, for its `pending`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Its head plays notes (a SYNTH, a plugin that activated as an instrument).
    Source,
    /// Its input feeds it (no head, an effect head).
    Input,
}

/// A slot's role as far as it is known now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeadRole {
    Known(Role),
    /// The head's plate has not applied yet (still to add, loading, `Waiting`, `Leaving`): read again
    /// at the next view.
    Wait,
    /// The head is `Held`: `pending` stays for the next launch.
    Keep,
}

/// Slot `slot`'s role from its record units (in record order), their dispositions and the slot's
/// plates (D9). A unit the plan dropped (`Forgotten`, `Yielded`) or no longer holds is not a head.
pub fn head_role(units: &[UnitRecord], dispositions: &BTreeMap<UnitId, Disposition>, plates: &[PlateView]) -> HeadRole {
    let head = units.iter().find(|u| !matches!(dispositions.get(&u.id), Some(Disposition::Forgotten | Disposition::Yielded)));
    let Some(head) = head else {
        // No record unit: the player's first plate decides, and an empty chain is input-fed.
        return plates.iter().find(|p| !p.failed_add).map_or(HeadRole::Known(Role::Input), plate_role);
    };
    match &head.body {
        UnitBody::Builtin { kind, .. } => HeadRole::Known(builtin_role(*kind)),
        UnitBody::Plugin(_) => match dispositions.get(&head.id) {
            Some(Disposition::Held { .. }) => HeadRole::Keep,
            Some(Disposition::Restored { token } | Disposition::Matched { token }) => {
                plates.iter().find(|p| p.added_by == *token).map_or(HeadRole::Wait, plate_role)
            }
            _ => HeadRole::Wait,
        },
    }
}

fn builtin_role(kind: UnitKind) -> Role {
    if kind == UnitKind::Synth {
        Role::Source
    } else {
        Role::Input
    }
}

/// A plate's role: a built-in's by kind; a plugin's from its `source` once applied, never before.
fn plate_role(plate: &PlateView) -> HeadRole {
    match plate.kind {
        _ if plate.state == PlateState::Leaving => HeadRole::Wait,
        UnitKind::Plugin if plate.applied => HeadRole::Known(if plate.source { Role::Source } else { Role::Input }),
        UnitKind::Plugin => HeadRole::Wait,
        kind => HeadRole::Known(builtin_role(kind)),
    }
}

/// What `role` takes from a slot's `pending`, in its order: all of it for an input-fed chain; for a
/// source-headed one the OTT alone, when it is on (its sends never reached a source's notes).
pub fn placed(pending: &[UnitRecord], role: Role) -> Vec<UnitRecord> {
    match role {
        Role::Input => pending.to_vec(),
        Role::Source => pending
            .iter()
            .filter(|u| matches!(u.body, UnitBody::Builtin { kind: UnitKind::Ott, .. }) && !u.bypassed())
            .cloned()
            .collect(),
    }
}

/// Where the record unit at `index` of its slot's `units` goes: after the record units before it that
/// the record still keeps, in record order (nearest last).
pub fn anchor(units: &[UnitRecord], dispositions: &BTreeMap<UnitId, Disposition>, index: usize) -> Place {
    Place::After(
        units[..index]
            .iter()
            .filter(|u| !matches!(dispositions.get(&u.id), Some(Disposition::Forgotten | Disposition::Yielded)))
            .map(|u| u.id)
            .collect(),
    )
}

/// Whether the model already holds a record unit (D9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Match {
    /// A live plate of its slot is this unit: never added; its plate carries this token.
    Matched(InstallToken),
    /// Another live plate holds its id (another kind or plugin, the other slot, or `Leaving`): the add
    /// would be refused.
    Taken,
    /// No live plate holds its id: add it.
    Free,
}

/// `unit` (of slot `slot`) against the model. A failed add's plate is neither a match nor in the way:
/// the add replaces it.
pub fn find_match(slot: usize, unit: &UnitRecord, view: &RigView) -> Match {
    let mut taken = false;
    for (s, plates) in view.slots.iter().enumerate() {
        for plate in plates.iter().filter(|p| p.id == unit.id && !p.failed_add) {
            if s == slot && plate.state != PlateState::Leaving && same_unit(unit, plate) {
                return Match::Matched(plate.added_by);
            }
            taken = true;
        }
    }
    if taken {
        Match::Taken
    } else {
        Match::Free
    }
}

/// Same kind and, for a plugin, the same `(format, path, id)`.
fn same_unit(unit: &UnitRecord, plate: &PlateView) -> bool {
    match &unit.body {
        UnitBody::Plugin(record) => plate.kind == UnitKind::Plugin && plate.plugin.as_ref() == Some(&record.plugin),
        UnitBody::Builtin { kind, .. } => plate.kind == *kind && plate.plugin.is_none(),
    }
}
