//! Placement's rules (D9) as pure reads, apart from the restore that acts on them
//! (`tests/restore.rs`): the head's role, what a role takes from `pending`, the anchor, the match.

use std::collections::BTreeMap;

use lf_rig::place::{anchor, find_match, head_role, placed};
use lf_rig::{
    Disposition, HeadRole, InstallToken, Match, Place, PlateState, PlateView, PluginRecord, PluginRefView, RigView, Role, UnitBody, UnitId,
    UnitKind, UnitRecord,
};

fn fx(id: UnitId, kind: UnitKind) -> UnitRecord {
    UnitRecord { id, body: UnitBody::Builtin { kind, params: None }, bypass: None }
}

fn plug(id: UnitId, path: &str) -> UnitRecord {
    let plugin = PluginRefView { format: "clap".into(), path: path.into(), id: "com.example".into() };
    UnitRecord { id, body: UnitBody::Plugin(PluginRecord { plugin, name: None }), bypass: None }
}

fn plate(unit: &UnitRecord, token: InstallToken, state: PlateState, applied: bool, source: bool) -> PlateView {
    let (kind, plugin) = match &unit.body {
        UnitBody::Plugin(p) => (UnitKind::Plugin, Some(p.plugin.clone())),
        UnitBody::Builtin { kind, .. } => (*kind, None),
    };
    PlateView { id: unit.id, added_by: token, kind, plugin, name: String::new(), params: None, bypass: false, source, state, applied, failed_add: state == PlateState::Failed }
}

#[test]
fn head_role_by_kind_applied_source_or_hold() {
    let none = BTreeMap::new();
    assert_eq!(head_role(&[], &none, &[]), HeadRole::Known(Role::Input), "an empty chain is input-fed");
    assert_eq!(head_role(&[fx(1, UnitKind::Synth)], &none, &[]), HeadRole::Known(Role::Source));
    assert_eq!(head_role(&[fx(1, UnitKind::Echo)], &none, &[]), HeadRole::Known(Role::Input));

    let p = plug(1, "a.clap");
    let mut d = BTreeMap::from([(1, Disposition::Queued)]);
    assert_eq!(head_role(std::slice::from_ref(&p), &d, &[]), HeadRole::Wait);
    d.insert(1, Disposition::Restored { token: 7 });
    assert_eq!(head_role(std::slice::from_ref(&p), &d, &[plate(&p, 7, PlateState::Waiting, false, true)]), HeadRole::Wait);
    assert_eq!(head_role(std::slice::from_ref(&p), &d, &[plate(&p, 7, PlateState::Seated, true, true)]), HeadRole::Known(Role::Source));
    d.insert(1, Disposition::Held { why: String::new(), token: None });
    assert_eq!(head_role(std::slice::from_ref(&p), &d, &[]), HeadRole::Keep);
    d.insert(1, Disposition::Forgotten);
    assert_eq!(head_role(&[p, fx(2, UnitKind::Synth)], &d, &[]), HeadRole::Known(Role::Source), "the next unit decides");
}

#[test]
fn a_source_takes_only_an_ott_that_is_on() {
    let off = UnitRecord { bypass: Some(true), ..fx(1, UnitKind::Ott) };
    let pending = [fx(2, UnitKind::Ott), fx(3, UnitKind::Ring), fx(4, UnitKind::Echo)];
    assert_eq!(placed(&pending, Role::Input), pending);
    assert_eq!(placed(&pending, Role::Source), [fx(2, UnitKind::Ott)]);
    assert!(placed(&[off], Role::Source).is_empty());
}

#[test]
fn anchor_lists_record_units_before_it_still_kept() {
    let units = [fx(1, UnitKind::Echo), fx(2, UnitKind::Ring), fx(3, UnitKind::Reverb)];
    let d = BTreeMap::from([(1, Disposition::Queued), (2, Disposition::Forgotten), (3, Disposition::Queued)]);
    assert_eq!(anchor(&units, &d, 0), Place::After(vec![]));
    assert_eq!(anchor(&units, &d, 2), Place::After(vec![1]));
}

#[test]
fn match_by_slot_id_kind_and_identity() {
    let p = plug(5, "a.clap");
    let at = |slot: usize, plate: PlateView| {
        let mut view = RigView::default();
        view.slots[slot].push(plate);
        view
    };
    assert_eq!(find_match(0, &p, &at(0, plate(&p, 9, PlateState::Seated, true, false))), Match::Matched(9));
    assert_eq!(find_match(0, &p, &at(0, plate(&p, 9, PlateState::Loading, false, false))), Match::Matched(9), "a live plate, not yet applied");
    assert_eq!(find_match(0, &p, &at(1, plate(&p, 9, PlateState::Seated, true, false))), Match::Taken, "the other slot");
    assert_eq!(find_match(0, &p, &at(0, plate(&plug(5, "b.clap"), 9, PlateState::Seated, true, false))), Match::Taken, "another plugin");
    assert_eq!(find_match(0, &p, &at(0, plate(&fx(5, UnitKind::Echo), 9, PlateState::Seated, true, false))), Match::Taken, "another kind");
    assert_eq!(find_match(0, &p, &at(0, plate(&p, 9, PlateState::Leaving, true, false))), Match::Taken, "leaving");
    assert_eq!(find_match(0, &p, &at(0, plate(&p, 9, PlateState::Failed, false, false))), Match::Free, "a failed add is replaced");
    assert_eq!(find_match(0, &p, &RigView::default()), Match::Free);
}
