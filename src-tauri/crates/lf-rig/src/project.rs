//! The record as a projection of the chain model (D3), what kind of change a new projection is to the
//! writer, and the page's import with DEV adoption (D12). Pure: the app's glue takes the settings' slot
//! gains, then `Model::rig_view` (each lock released before the next), and calls these with the
//! restore's [`Plan`]; the [`crate::store::Store`] writes what they return.
//!
//! **What a projection holds, per slot in plate order:** each plate the engine applied (a `Leaving`
//! one too, until it is gone: a refused removal reverts it), and each plate whose install token
//! carries a live disposition of the restore; then each record unit the restore still holds (`Queued`,
//! `Issued`, `Held`) at its record place, whatever its plate shows; `pending` as the plan holds it;
//! each slot's volume from the settings memory when it holds one, else the record's (D2). A fresh add
//! that has not applied is not recorded (D4); nor is a plate that cannot be a record unit (a legacy
//! row's shape: a plugin plate with no plugin identity, which `rig_view` already leaves out).

use std::collections::BTreeSet;

use lf_engine::{InstallToken, UnitId, UnitKind, UnitParam};

use crate::plan::{Disposition, Plan};
use crate::record::{PluginRecord, RigRecord, SlotRecord, UnitBody, UnitRecord, Unreadable, Version1};
use crate::view::{PlateView, RigView};

/// Each slot's gain as the settings memory holds it, A then B: `None` until a gesture or the rig's
/// seed set it in this process (D2). The glue reads them under `settings` just before it projects.
pub type Volumes = [Option<f64>; 2];

/// Each slot's volume as the fader shows it and every durable write takes it (D2): the settings
/// memory's, else the record's. `RigAnswer::volumes` on `present`.
pub fn volumes(record: &RigRecord, held: Volumes) -> [f64; 2] {
    [0, 1].map(|s| held[s].unwrap_or(record.slots[s].volume))
}

/// The record the chain model stands for now (D3; the module doc). `record` is the last one stored:
/// it gives a slot's volume when the settings memory holds none, and a unit whose values did not move
/// keeps its stored form (an omitted `bypass`, `params: {}`), so a projection of an unchanged chain is
/// the stored record itself.
pub fn project(record: &RigRecord, plan: &Plan, view: &RigView, held: Volumes) -> RigRecord {
    let tokens: BTreeSet<InstallToken> = view.slots.iter().flatten().map(|p| p.added_by).collect();
    let volumes = volumes(record, held);
    let slots = [0, 1].map(|s| {
        let mut units = Vec::new();
        for plate in &view.slots[s] {
            if recorded(plate, plan.dispositions.get(&plate.id), &tokens) {
                if let Some(unit) = unit_of(plate, prior(record, plan, plate.id)) {
                    units.push(unit);
                }
            }
        }
        // The units the restore still holds, each after the nearest unit before it in the plan that
        // the output holds (none: at the head), in plan order so an earlier held unit counts.
        let order = &plan.units[s];
        for (i, unit) in order.iter().enumerate() {
            let holds = plan.dispositions.get(&unit.id).is_some_and(|d| holding(d, &tokens));
            if !holds || units.iter().any(|u| u.id == unit.id) {
                continue;
            }
            let at = order[..i]
                .iter()
                .rev()
                .find_map(|before| units.iter().position(|u| u.id == before.id))
                .map_or(0, |pos| pos + 1);
            units.insert(at, unit.clone());
        }
        SlotRecord { volume: volumes[s], units, pending: plan.pending[s].clone() }
    });
    RigRecord { version: Version1, slots }
}

/// Whether a disposition keeps its record unit at its place, whatever its plate shows: `Queued`, and
/// an `Issued` or `Held` until a confirmed removal ends it (the plate carrying its token left the
/// model; with no token, nothing of it is in the model to remove).
fn holding(d: &Disposition, tokens: &BTreeSet<InstallToken>) -> bool {
    match d {
        Disposition::Queued => true,
        Disposition::Issued { token } | Disposition::Held { token, .. } => token.is_none_or(|t| tokens.contains(&t)),
        Disposition::Restored { .. } | Disposition::Matched { .. } | Disposition::Forgotten | Disposition::Yielded => false,
    }
}

/// Whether a plate is recorded from the model. A disposition wins over the plate's state: a holding
/// one writes the plan's unit instead (so the id is written once), `Restored`/`Matched` record the
/// plate their token names whatever its state, `Forgotten`/`Yielded` record nothing under the id. A
/// plate no live disposition names is recorded once applied (D4), `Leaving` included.
fn recorded(plate: &PlateView, d: Option<&Disposition>, tokens: &BTreeSet<InstallToken>) -> bool {
    match d {
        Some(Disposition::Forgotten | Disposition::Yielded) => false,
        Some(Disposition::Restored { token } | Disposition::Matched { token }) if *token == plate.added_by => true,
        Some(d) if holding(d, tokens) => false,
        _ => plate.applied,
    }
}

/// The stored form of a unit by id: the last record's, else the plan's.
fn prior<'a>(record: &'a RigRecord, plan: &'a Plan, id: UnitId) -> Option<&'a UnitRecord> {
    let stored = record.slots.iter().flat_map(|s| s.units.iter());
    let planned = plan.units.iter().flatten();
    stored.chain(planned).find(|u| u.id == id)
}

/// A plate as a record unit, as the page's `recordOf` wrote it (`bypass` always, `params` when the
/// plate has them, the plugin's name); `prior` instead when it says the same. `None` for a plugin
/// plate with no plugin identity: a legacy row, never recorded.
fn unit_of(plate: &PlateView, prior: Option<&UnitRecord>) -> Option<UnitRecord> {
    let body = match (plate.kind, &plate.plugin) {
        (UnitKind::Plugin, None) => return None,
        (UnitKind::Plugin, Some(plugin)) => UnitBody::Plugin(PluginRecord { plugin: plugin.clone(), name: Some(plate.name.clone()) }),
        (kind, _) => UnitBody::Builtin { kind, params: plate.params.clone() },
    };
    let fresh = UnitRecord { id: plate.id, body, bypass: Some(plate.bypass) };
    match prior {
        Some(prior) if same_unit(prior, &fresh) => Some(prior.clone()),
        _ => Some(fresh),
    }
}

/// What kind of change a new projection is against the last one stored (D3): the writer stores a
/// structure change at its next tick, a value change once the values have been quiet for
/// [`crate::store::SAVE_QUIET`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Change {
    /// The same record in meaning (field presence and a parameter's place in the list aside).
    None,
    /// Only values: a volume, a parameter, a bypass, a plugin's name.
    Values,
    /// Units, their order or kinds, or `pending`.
    Structure,
}

/// How `new` differs from `old` ([`Change`]).
pub fn change(old: &RigRecord, new: &RigRecord) -> Change {
    let mut change = Change::None;
    for (a, b) in old.slots.iter().zip(&new.slots) {
        let same_shape = a.units.len() == b.units.len() && a.units.iter().zip(&b.units).all(|(x, y)| x.id == y.id && identity(x) == identity(y));
        if !same_shape || a.pending.len() != b.pending.len() || !a.pending.iter().zip(&b.pending).all(|(x, y)| same_unit(x, y)) {
            return Change::Structure;
        }
        if !same(a.volume, b.volume) || !a.units.iter().zip(&b.units).all(|(x, y)| same_unit(x, y)) {
            change = Change::Values;
        }
    }
    change
}

/// What a unit is, apart from its values: its kind and, for a plugin, its identity.
fn identity(unit: &UnitRecord) -> (UnitKind, Option<(&str, &str, &str)>) {
    match &unit.body {
        UnitBody::Plugin(p) => (UnitKind::Plugin, Some((p.plugin.format.as_str(), p.plugin.path.as_str(), p.plugin.id.as_str()))),
        UnitBody::Builtin { kind, .. } => (*kind, None),
    }
}

/// Two units equal in meaning: an omitted `bypass` is false, omitted `params` none, parameters in any
/// order, a plugin name compared only when both carry one.
fn same_unit(a: &UnitRecord, b: &UnitRecord) -> bool {
    a.id == b.id
        && a.bypassed() == b.bypassed()
        && match (&a.body, &b.body) {
            (UnitBody::Plugin(x), UnitBody::Plugin(y)) => x.plugin == y.plugin && (x.name.is_none() || y.name.is_none() || x.name == y.name),
            (UnitBody::Builtin { kind: k, params: p }, UnitBody::Builtin { kind: l, params: q }) => k == l && same_params(p.as_deref(), q.as_deref()),
            _ => false,
        }
}

fn same_params(a: Option<&[(UnitParam, f64)]>, b: Option<&[(UnitParam, f64)]>) -> bool {
    let (a, b) = (a.unwrap_or_default(), b.unwrap_or_default());
    let holds = |x: &[(UnitParam, f64)], y: &[(UnitParam, f64)]| {
        x.iter().all(|(p, v)| y.iter().any(|(q, w)| p == q && same(*v, *w)))
    };
    a.len() == b.len() && holds(a, b) && holds(b, a)
}

/// Equal numbers, NaN equal to itself (a NaN level would otherwise read as a change at every tick).
fn same(a: f64, b: f64) -> bool {
    a == b || (a.is_nan() && b.is_nan())
}

/// Why an import was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportError {
    /// The page's text is not a v1 record.
    Unreadable(Unreadable),
    /// The model already holds a unit under this import id (D12): nothing is stored.
    IdHeld(UnitId),
}

/// The page's upgrade as the record to store (D12): read as a file is ([`RigRecord::read`]); each
/// slot whose model holds an applied unit takes the model's applied units, their ids and order, in
/// place of the import's (DEV adoption: a page that ran chain mode before this build added them),
/// keeping the import's `pending`; each slot's volume per D2. An import id any plate of the model
/// holds refuses the import. `view` is a fresh `rig_view`, taken for this import.
pub fn import(text: &str, view: &RigView, held: Volumes) -> Result<RigRecord, ImportError> {
    let mut record = RigRecord::read(text.as_bytes()).map_err(ImportError::Unreadable)?;
    let plates: BTreeSet<UnitId> = view.slots.iter().flatten().map(|p| p.id).collect();
    let imported = record.slots.iter().flat_map(|s| s.units.iter().chain(&s.pending));
    if let Some(id) = imported.map(|u| u.id).find(|id| plates.contains(id)) {
        return Err(ImportError::IdHeld(id));
    }
    let volumes = volumes(&record, held);
    for (s, slot) in record.slots.iter_mut().enumerate() {
        let adopted: Vec<UnitRecord> = view.slots[s].iter().filter(|p| p.applied).filter_map(|p| unit_of(p, None)).collect();
        if !adopted.is_empty() {
            slot.units = adopted;
        }
        slot.volume = volumes[s];
    }
    Ok(record)
}
