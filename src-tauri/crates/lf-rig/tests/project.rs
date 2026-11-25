//! The record as a projection of the chain model (D3, D4), a change's kind for the writer, the slot
//! volume's rule (D2) and the import's adoption (D12). Views here are what `Model::rig_view` hands the
//! rig; that the model sets `applied` and `added_by` as these assume is CP2b's to prove on the model.

use std::collections::BTreeMap;

use lf_rig::{
    change, import, project, volumes, Change, Disposition, ImportError, Plan, PlateState, PlateView, PluginRecord, PluginRefView, RigRecord, RigView,
    SlotRecord, UnitBody, UnitId, UnitKind, UnitParam, UnitRecord, Version1,
};

// ── builders ─────────────────────────────────────────────────────────────────────────────────────────

fn level() -> UnitParam {
    UnitParam::from_key("echoLevel").expect("a key")
}

fn echo(id: UnitId, value: f64) -> UnitRecord {
    UnitRecord { id, body: UnitBody::Builtin { kind: UnitKind::Echo, params: Some(vec![(level(), value)]) }, bypass: Some(false) }
}

fn amp() -> PluginRefView {
    PluginRefView { format: "vst3".into(), path: "C:\\Amp.vst3".into(), id: "AMP".into() }
}

/// A seated, applied ECHO plate at `value`, added under `token`.
fn plate(id: UnitId, token: u64, value: f64) -> PlateView {
    PlateView {
        id,
        added_by: token,
        kind: UnitKind::Echo,
        plugin: None,
        name: "ECHO".into(),
        params: Some(vec![(level(), value)]),
        bypass: false,
        source: false,
        state: PlateState::Seated,
        applied: true,
        failed_add: false,
    }
}

fn with(mut p: PlateView, state: PlateState, applied: bool) -> PlateView {
    p.state = state;
    p.applied = applied;
    p.failed_add = state == PlateState::Failed && !applied;
    p
}

fn view(a: Vec<PlateView>) -> RigView {
    RigView { rev: 1, slots: [a, vec![]] }
}

fn record(a: Vec<UnitRecord>) -> RigRecord {
    RigRecord { version: Version1, slots: [SlotRecord { volume: 0.8, units: a, pending: vec![] }, SlotRecord { volume: 1.0, units: vec![], pending: vec![] }] }
}

fn plan(a: Vec<UnitRecord>, dispositions: Vec<(UnitId, Disposition)>) -> Plan {
    Plan { units: [a, vec![]], pending: [vec![], vec![]], dispositions: dispositions.into_iter().collect::<BTreeMap<_, _>>() }
}

const NONE: [Option<f64>; 2] = [None, None];

/// Slot A's unit ids as projected.
fn ids(r: &RigRecord, plan_: &Plan, v: &RigView) -> Vec<UnitId> {
    project(r, plan_, v, NONE).slots[0].units.iter().map(|u| u.id).collect()
}

// ── the projection ───────────────────────────────────────────────────────────────────────────────────

#[test]
fn a_leaving_plate_stays_until_gone() {
    let r = record(vec![echo(1, 0.5), echo(2, 0.5)]);
    let p = Plan::default();
    assert_eq!(ids(&r, &p, &view(vec![plate(1, 10, 0.5), with(plate(2, 20, 0.5), PlateState::Leaving, true)])), [1, 2]);
    assert_eq!(ids(&r, &p, &view(vec![plate(1, 10, 0.5)])), [1], "gone: a confirmed removal");
}

#[test]
fn a_refused_removal_keeps_the_unit() {
    let r = record(vec![echo(1, 0.5), echo(2, 0.5)]);
    let p = Plan::default();
    for state in [PlateState::Seated, PlateState::Leaving, PlateState::Seated] {
        assert_eq!(ids(&r, &p, &view(vec![plate(1, 10, 0.5), with(plate(2, 20, 0.5), state, true)])), [1, 2], "{state:?}");
    }
}

#[test]
fn legacy_plates_are_never_recorded() {
    // A legacy row's shape (chain.rs `Spec::Legacy`): a plugin plate with no plugin identity. `rig_view`
    // leaves them out; the projection never writes one either, applied or named by a disposition.
    let mut legacy = plate(3, 30, 0.0);
    legacy.kind = UnitKind::Plugin;
    legacy.params = None;
    let r = record(vec![]);
    assert_eq!(ids(&r, &Plan::default(), &view(vec![plate(1, 10, 0.5), legacy.clone()])), [1]);
    let restored = plan(vec![], vec![(3, Disposition::Restored { token: 30 })]);
    assert_eq!(ids(&r, &restored, &view(vec![legacy])), Vec::<UnitId>::new());
}

#[test]
fn a_fresh_waiting_or_reseating_add_is_not_recorded() {
    let r = record(vec![echo(1, 0.5)]);
    let p = Plan::default();
    // A first install held by a take.
    assert_eq!(ids(&r, &p, &view(vec![plate(1, 10, 0.5), with(plate(2, 20, 0.5), PlateState::Waiting, false)])), [1]);
    // A rebuild reseating a first install that never applied (`reseating_since`): it reads seated.
    assert_eq!(ids(&r, &p, &view(vec![plate(1, 10, 0.5), with(plate(2, 20, 0.5), PlateState::Seated, false)])), [1]);
    // Still loading.
    assert_eq!(ids(&r, &p, &view(vec![plate(1, 10, 0.5), with(plate(2, 20, 0.5), PlateState::Loading, false)])), [1]);
    // Once applied, it enters.
    assert_eq!(ids(&r, &p, &view(vec![plate(1, 10, 0.5), plate(2, 20, 0.5)])), [1, 2]);
}

#[test]
fn applied_survives_reload_park_and_withdrawal() {
    // `applied` is never cleared, so an applied plate is recorded in every state the model shows it in:
    // a reload or a park (seated), a rebuild reseating it (waiting or loading), a seat lost (failed).
    let r = record(vec![echo(1, 0.5)]);
    for state in [PlateState::Seated, PlateState::Waiting, PlateState::Loading, PlateState::Failed] {
        assert_eq!(ids(&r, &Plan::default(), &view(vec![with(plate(1, 10, 0.5), state, true)])), [1], "{state:?}");
    }
}

#[test]
fn a_reused_id_does_not_inherit_applied() {
    let r = record(vec![echo(1, 0.5)]);
    // The restore seated id 1 under token 10; that plate failed and was dropped, and a new add of id 1
    // (token 11) has not applied.
    let p = plan(vec![echo(1, 0.5)], vec![(1, Disposition::Restored { token: 10 })]);
    assert_eq!(ids(&r, &p, &view(vec![with(plate(1, 11, 0.5), PlateState::Waiting, false)])), Vec::<UnitId>::new());
    assert_eq!(ids(&r, &p, &view(vec![plate(1, 11, 0.5)])), [1], "applied, the new plate enters on its own");
}

#[test]
fn a_disposition_wins_over_a_failed_plate() {
    let r = record(vec![echo(1, 0.5), echo(2, 0.5)]);
    // Matched (the model held it) and Restored plates whose seat failed since: still recorded, at place.
    let p = plan(vec![echo(1, 0.5), echo(2, 0.5)], vec![(1, Disposition::Matched { token: 10 }), (2, Disposition::Restored { token: 20 })]);
    let v = view(vec![with(plate(1, 10, 0.5), PlateState::Failed, false), with(plate(2, 20, 0.5), PlateState::Failed, false)]);
    assert_eq!(ids(&r, &p, &v), [1, 2]);
    // Issued: an add whose plate failed with no answer yet is kept from the plan, at its place.
    let p = plan(vec![echo(1, 0.5), echo(2, 0.5)], vec![(1, Disposition::Issued { token: Some(10) }), (2, Disposition::Queued)]);
    assert_eq!(ids(&r, &p, &view(vec![with(plate(1, 10, 0.5), PlateState::Failed, false)])), [1, 2]);
}

#[test]
fn forgotten_never_returns_through_a_late_failure() {
    // An `Issued` add went `Waiting`, then its plugin failed: `Forgotten`. Its failed plate stays
    // visible; however it shows later, the unit is not recorded again.
    let r = record(vec![echo(1, 0.5), echo(2, 0.5)]);
    let p = plan(vec![echo(1, 0.5), echo(2, 0.5)], vec![(1, Disposition::Forgotten), (2, Disposition::Yielded)]);
    for (state, applied) in [(PlateState::Failed, false), (PlateState::Seated, true)] {
        let v = view(vec![with(plate(1, 10, 0.5), state, applied), with(plate(2, 20, 0.5), state, applied)]);
        assert_eq!(ids(&r, &p, &v), Vec::<UnitId>::new(), "{state:?}");
    }
    assert_eq!(ids(&r, &p, &view(vec![])), Vec::<UnitId>::new());
}

#[test]
fn held_with_a_plate_is_written_once() {
    let r = record(vec![echo(1, 0.5), echo(2, 0.5)]);
    // A seat refusal's plate (applied in a view, so a plate rule alone would record it too).
    let held = Disposition::Held { why: "seat refused".into(), token: Some(10) };
    let p = plan(vec![echo(1, 0.5), echo(2, 0.5)], vec![(1, held), (2, Disposition::Queued)]);
    let v = view(vec![with(plate(1, 10, 0.9), PlateState::Failed, true), plate(2, 20, 0.7)]);
    let out = project(&r, &p, &v, NONE);
    assert_eq!(out.slots[0].units, [echo(1, 0.5), echo(2, 0.5)], "each once, as the plan holds it, at its place");
}

#[test]
fn a_confirmed_removal_ends_a_disposition() {
    let r = record(vec![echo(1, 0.5), echo(2, 0.5), echo(3, 0.5)]);
    let units = vec![echo(1, 0.5), echo(2, 0.5), echo(3, 0.5)];
    let p = plan(units, vec![
        (1, Disposition::Held { why: "seat refused".into(), token: Some(10) }),
        (2, Disposition::Restored { token: 20 }),
        (3, Disposition::Issued { token: Some(30) }),
    ]);
    let all = view(vec![with(plate(1, 10, 0.5), PlateState::Failed, false), plate(2, 20, 0.5), with(plate(3, 30, 0.5), PlateState::Waiting, false)]);
    assert_eq!(ids(&r, &p, &all), [1, 2, 3]);
    // The player removed each plate: the units leave the record.
    assert_eq!(ids(&r, &p, &view(vec![])), Vec::<UnitId>::new());
    // A `Held` that never had a plate stays: nothing of it was in the model to remove.
    let p = plan(vec![echo(1, 0.5)], vec![(1, Disposition::Held { why: "not in the scan".into(), token: None })]);
    assert_eq!(ids(&r, &p, &view(vec![])), [1]);
}

#[test]
fn plan_units_without_a_plate_keep_their_record_place() {
    let r = record(vec![]);
    let units = vec![echo(1, 0.5), echo(2, 0.5), echo(3, 0.5), echo(4, 0.5)];
    let p = plan(units, vec![
        (1, Disposition::Queued),
        (2, Disposition::Restored { token: 20 }),
        (3, Disposition::Held { why: "missing".into(), token: None }),
        (4, Disposition::Restored { token: 40 }),
    ]);
    // The player added 9 at the head meanwhile.
    let v = view(vec![plate(9, 90, 0.5), plate(2, 20, 0.5), plate(4, 40, 0.5)]);
    assert_eq!(ids(&r, &p, &v), [1, 9, 2, 3, 4], "1 has no unit before it: the head; 3 after 2");
    // 2 removed by the player: 3 goes after the nearest earlier plan unit present, 1.
    assert_eq!(ids(&r, &p, &view(vec![plate(9, 90, 0.5), plate(4, 40, 0.5)])), [1, 3, 9, 4]);
}

#[test]
fn pending_is_as_the_plan_holds_it() {
    let r = record(vec![]);
    let mut p = plan(vec![], vec![]);
    p.pending[1] = vec![echo(5, 0.2)];
    let out = project(&r, &p, &view(vec![]), NONE);
    assert_eq!(out.slots[1].pending, [echo(5, 0.2)]);
    assert!(out.slots[0].pending.is_empty());
}

#[test]
fn an_unchanged_chain_projects_the_stored_record() {
    // A unit stored with an omitted `bypass` and `params: {}`, its plate the same in meaning: the
    // projection keeps the stored form, so the writer has nothing to write.
    let stored = UnitRecord { id: 1, body: UnitBody::Builtin { kind: UnitKind::Ring, params: Some(vec![]) }, bypass: None };
    let r = record(vec![stored.clone()]);
    let mut ring = plate(1, 10, 0.0);
    ring.kind = UnitKind::Ring;
    ring.params = None;
    let out = project(&r, &Plan::default(), &view(vec![ring]), NONE);
    assert_eq!(out, r);
}

#[test]
fn settings_volume_in_every_write() {
    let r = record(vec![echo(1, 0.5)]);
    // While the restore waits (a unit still queued) and after: the settings memory's level, else the record's.
    let p = plan(vec![echo(1, 0.5)], vec![(1, Disposition::Queued)]);
    let out = project(&r, &p, &view(vec![]), [None, Some(0.3)]);
    assert_eq!([out.slots[0].volume, out.slots[1].volume], [0.8, 0.3]);
    let out = project(&r, &Plan::default(), &view(vec![plate(1, 10, 0.5)]), [Some(1.2), None]);
    assert_eq!([out.slots[0].volume, out.slots[1].volume], [1.2, 1.0]);
    assert_eq!(volumes(&r, [Some(0.0), None]), [0.0, 1.0], "the answer's volumes: a gesture to 0 wins too");
    // The import.
    let text = serde_json::to_string(&r).expect("serializes");
    let imported = import(&text, &view(vec![]), [Some(0.4), None]).expect("imports");
    assert_eq!([imported.slots[0].volume, imported.slots[1].volume], [0.4, 1.0]);
}

// ── a change's kind ──────────────────────────────────────────────────────────────────────────────────

#[test]
fn params_are_values_order_is_structure() {
    let base = record(vec![echo(1, 0.5), echo(2, 0.5)]);
    assert_eq!(change(&base, &base.clone()), Change::None);
    assert_eq!(change(&base, &record(vec![echo(1, 0.6), echo(2, 0.5)])), Change::Values, "a parameter");
    let mut bypassed = base.clone();
    bypassed.slots[0].units[1].bypass = Some(true);
    assert_eq!(change(&base, &bypassed), Change::Values, "a bypass");
    let mut louder = base.clone();
    louder.slots[1].volume = 0.2;
    assert_eq!(change(&base, &louder), Change::Values, "a volume");
    assert_eq!(change(&base, &record(vec![echo(2, 0.5), echo(1, 0.5)])), Change::Structure, "the order");
    assert_eq!(change(&base, &record(vec![echo(1, 0.5)])), Change::Structure, "a unit removed");
    assert_eq!(change(&base, &record(vec![echo(1, 0.9), echo(2, 0.5), echo(3, 0.5)])), Change::Structure, "a unit added, with a value");
    let mut placed = base.clone();
    placed.slots[1].pending = vec![echo(7, 0.5)];
    assert_eq!(change(&base, &placed), Change::Structure, "pending");
    // Field presence and a parameter's place in the list are not changes.
    let mut omitted = base.clone();
    omitted.slots[0].units[0].bypass = None;
    assert_eq!(change(&base, &omitted), Change::None);
    let two = |a, b| UnitRecord { id: 1, body: UnitBody::Builtin { kind: UnitKind::Ott, params: Some(vec![a, b]) }, bypass: None };
    let (x, y) = ((UnitParam::from_key("ottDepth").expect("a key"), 0.1), (UnitParam::from_key("ottTime").expect("a key"), 0.2));
    assert_eq!(change(&record(vec![two(x, y)]), &record(vec![two(y, x)])), Change::None);
    // A plugin swapped for another under the same id is structure.
    let plugin = |path: &str| UnitRecord {
        id: 1,
        body: UnitBody::Plugin(PluginRecord { plugin: PluginRefView { path: path.into(), ..amp() }, name: None }),
        bypass: None,
    };
    assert_eq!(change(&record(vec![plugin("a")]), &record(vec![plugin("b")])), Change::Structure);
}

// ── the import ───────────────────────────────────────────────────────────────────────────────────────

fn amp_plate(id: UnitId, token: u64, applied: bool) -> PlateView {
    PlateView {
        id,
        added_by: token,
        kind: UnitKind::Plugin,
        plugin: Some(amp()),
        name: "Amp Sim".into(),
        params: None,
        bypass: true,
        source: false,
        state: if applied { PlateState::Seated } else { PlateState::Loading },
        applied,
        failed_add: false,
    }
}

#[test]
fn import_adopts_the_model_chain_fresh() {
    let mut upgraded = record(vec![echo(1, 0.5)]);
    upgraded.slots[0].pending = vec![echo(2, 0.3)];
    upgraded.slots[1].units = vec![echo(3, 0.5)];
    let text = serde_json::to_string(&upgraded).expect("serializes");
    // Slot A's model holds an applied amp and a fresh add not yet applied; slot B's holds nothing.
    let v = view(vec![amp_plate(40, 400, true), with(plate(41, 410, 0.5), PlateState::Waiting, false)]);
    let out = import(&text, &v, [None, Some(0.6)]).expect("imports");
    let [a, b] = &out.slots;
    assert_eq!(a.units.len(), 1);
    assert_eq!(a.units[0].id, 40, "the model's applied units, in place of the import's");
    match &a.units[0].body {
        UnitBody::Plugin(p) => assert_eq!((&p.plugin, p.name.as_deref()), (&amp(), Some("Amp Sim"))),
        other => panic!("the amp, got {other:?}"),
    }
    assert!(a.units[0].bypassed());
    assert_eq!(a.pending, [echo(2, 0.3)], "the import's pending kept");
    assert_eq!(a.volume, 0.8);
    assert_eq!(b.units, [echo(3, 0.5)], "a slot whose model holds nothing keeps the import's units");
    assert_eq!(b.volume, 0.6);
}

#[test]
fn import_refuses_an_id_the_model_holds() {
    let mut upgraded = record(vec![echo(1, 0.5)]);
    upgraded.slots[1].pending = vec![echo(2, 0.3)];
    let text = serde_json::to_string(&upgraded).expect("serializes");
    // The id in slot B's pending is held by a plate in slot A, not even applied.
    assert_eq!(import(&text, &view(vec![with(plate(2, 20, 0.5), PlateState::Loading, false)]), NONE), Err(ImportError::IdHeld(2)));
    assert_eq!(import(&text, &view(vec![plate(1, 10, 0.5)]), NONE), Err(ImportError::IdHeld(1)));
    assert!(matches!(import("{", &view(vec![]), NONE), Err(ImportError::Unreadable(_))));
    assert!(import(&text, &view(vec![plate(5, 50, 0.5)]), NONE).is_ok());
}
