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

/// A plan whose dispositions' plates an earlier view showed (each token named is `seen`).
fn plan(a: Vec<UnitRecord>, dispositions: Vec<(UnitId, Disposition)>) -> Plan {
    let seen = dispositions
        .iter()
        .filter_map(|(_, d)| match d {
            Disposition::Restored { token } | Disposition::Matched { token } => Some(*token),
            Disposition::Issued { token } | Disposition::Held { token, .. } => *token,
            Disposition::Queued | Disposition::Forgotten | Disposition::Yielded => None,
        })
        .collect();
    Plan { units: [a, vec![]], pending: [vec![], vec![]], dispositions: dispositions.into_iter().collect::<BTreeMap<_, _>>(), seen }
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
    // The plugin's path (D5): its add is out (`Issued`) and its plate waits; then the plugin's own load
    // fails, so the restore forgets it. Its failed plate stays visible, never applied.
    let r = record(vec![echo(1, 0.5), echo(2, 0.5)]);
    let units = vec![echo(1, 0.5), echo(2, 0.5)];
    let issued = plan(units.clone(), vec![(1, Disposition::Issued { token: Some(10) }), (2, Disposition::Queued)]);
    assert_eq!(ids(&r, &issued, &view(vec![with(plate(1, 10, 0.5), PlateState::Waiting, false)])), [1, 2], "issued, waiting: held at its place");
    let failed = with(plate(1, 10, 0.5), PlateState::Failed, false);
    let forgotten = plan(units.clone(), vec![(1, Disposition::Forgotten), (2, Disposition::Queued)]);
    assert_eq!(ids(&r, &forgotten, &view(vec![failed.clone()])), [2], "failed: gone from the record");
    assert_eq!(ids(&r, &forgotten, &view(vec![])), [2], "and still gone once its plate is removed");
    // A yielded unit's refused plate is the same.
    let yielded = plan(units, vec![(1, Disposition::Yielded), (2, Disposition::Queued)]);
    assert_eq!(ids(&r, &yielded, &view(vec![failed])), [2]);
}

/// Each id's rows in a record: units and pending, both slots.
fn rows(r: &RigRecord) -> BTreeMap<UnitId, usize> {
    let mut rows = BTreeMap::new();
    for unit in r.slots.iter().flat_map(|s| s.units.iter().chain(&s.pending)) {
        *rows.entry(unit.id).or_insert(0) += 1;
    }
    rows
}

#[test]
fn held_with_a_plate_is_written_once() {
    let r = record(vec![echo(1, 0.5), echo(2, 0.5)]);
    // 1: held, with its own seat refusal's plate (applied in a view, so a plate rule alone would
    // record it too). 2: queued, and the player put an applied unit under its id meanwhile. 5: slot
    // A's pending, and slot B holds an applied unit under its id.
    let held = Disposition::Held { why: "seat refused".into(), token: Some(10) };
    let mut p = plan(vec![echo(1, 0.5), echo(2, 0.5)], vec![(1, held), (2, Disposition::Queued)]);
    p.pending[0] = vec![echo(5, 0.2)];
    let v = RigView { rev: 1, slots: [vec![with(plate(1, 10, 0.9), PlateState::Failed, true), plate(2, 20, 0.7)], vec![plate(5, 50, 0.4)]] };
    let out = project(&r, &p, &v, NONE);
    assert!(rows(&out).values().all(|&n| n == 1), "one row per id: {:?}", rows(&out));
    assert_eq!(out.slots[0].units, [echo(1, 0.5), echo(2, 0.7)], "the held unit as the plan holds it; the player's unit wins its id");
    assert_eq!(out.slots[1].units, [echo(5, 0.4)]);
}

#[test]
fn an_applied_plate_of_another_token_wins_a_held_id() {
    let r = record(vec![echo(1, 0.5), echo(2, 0.5), echo(3, 0.5)]);
    let units = vec![echo(1, 0.5), echo(2, 0.5), echo(3, 0.5)];
    let p = plan(units, vec![
        (1, Disposition::Queued),
        (2, Disposition::Held { why: "not in the scan".into(), token: None }),
        (3, Disposition::Forgotten),
    ]);
    // The player's adds under these ids, not applied yet: the held units keep their place, the plates
    // are not recorded.
    let fresh = |id, token| with(plate(id, token, 0.9), PlateState::Waiting, false);
    let out = project(&r, &p, &view(vec![fresh(1, 11), fresh(2, 21), fresh(3, 31)]), NONE);
    assert_eq!(out.slots[0].units, [echo(1, 0.5), echo(2, 0.5)]);
    // Applied: each is the player's unit, recorded from its plate (a forgotten id included).
    let out = project(&r, &p, &view(vec![plate(1, 11, 0.9), plate(2, 21, 0.9), plate(3, 31, 0.9)]), NONE);
    assert_eq!(out.slots[0].units, [echo(1, 0.9), echo(2, 0.9), echo(3, 0.9)]);
}

#[test]
fn a_held_unit_survives_its_failed_plates_removal_by_an_id_reuse() {
    let r = record(vec![echo(1, 0.5), echo(2, 0.5)]);
    let held = Disposition::Held { why: "seat refused".into(), token: Some(10) };
    let p = plan(vec![echo(1, 0.5), echo(2, 0.5)], vec![(1, held), (2, Disposition::Restored { token: 20 })]);
    // The player's add of id 1 dropped the failed plate (chain.rs `add_plate`); the new plate waits.
    let v = view(vec![with(plate(1, 11, 0.9), PlateState::Waiting, false), plate(2, 20, 0.5)]);
    assert_eq!(project(&r, &p, &v, NONE).slots[0].units, [echo(1, 0.5), echo(2, 0.5)], "still held, at its place");
    let v = view(vec![plate(1, 11, 0.9), plate(2, 20, 0.5)]);
    assert_eq!(project(&r, &p, &v, NONE).slots[0].units, [echo(1, 0.9), echo(2, 0.5)], "applied: the player's unit wins the id");
    assert_eq!(ids(&r, &p, &view(vec![plate(2, 20, 0.5)])), [2], "the new plate removed too: a confirmed removal");
}

#[test]
fn a_b_plate_with_an_a_pending_id_writes_the_id_once() {
    let r = record(vec![]);
    let mut p = plan(vec![], vec![]);
    p.pending[0] = vec![echo(5, 0.2), echo(6, 0.2)];
    let b = |plate| RigView { rev: 1, slots: [vec![], vec![plate]] };
    let out = project(&r, &p, &b(plate(5, 50, 0.4)), NONE);
    assert_eq!(out.slots[1].units, [echo(5, 0.4)]);
    assert_eq!(out.slots[0].pending, [echo(6, 0.2)], "the pending unit whose id a recorded unit holds is left out");
    let out = project(&r, &p, &b(with(plate(5, 50, 0.4), PlateState::Waiting, false)), NONE);
    assert!(out.slots[1].units.is_empty(), "not applied: not recorded");
    assert_eq!(out.slots[0].pending, [echo(5, 0.2), echo(6, 0.2)]);
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
fn a_renamed_plugin_is_a_value_change() {
    // A stored plugin unit without a name (or with an older one): the plate's name is written, as the
    // page's `recordOf` would, and the writer takes it as a value change.
    for name in [None, Some("Amp Sim (old)")] {
        let stored = UnitRecord { id: 1, body: UnitBody::Plugin(PluginRecord { plugin: amp(), name: name.map(str::to_owned) }), bypass: Some(true) };
        let r = record(vec![stored]);
        let out = project(&r, &Plan::default(), &view(vec![amp_plate(1, 10, true)]), NONE);
        match &out.slots[0].units[0].body {
            UnitBody::Plugin(p) => assert_eq!(p.name.as_deref(), Some("Amp Sim"), "{name:?}"),
            other => panic!("the amp, got {other:?}"),
        }
        assert_eq!(change(&r, &out), Change::Values, "{name:?}");
    }
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
    // Slot A's model holds an applied amp, a fresh add not yet applied and an applied ECHO; slot B's
    // holds nothing.
    let v = view(vec![amp_plate(40, 400, true), with(plate(41, 410, 0.5), PlateState::Waiting, false), plate(42, 420, 0.7)]);
    let out = import(&text, &v, [None, Some(0.6)]).expect("imports");
    let [a, b] = &out.slots;
    assert_eq!(a.units.iter().map(|u| u.id).collect::<Vec<_>>(), [40, 42], "the model's applied units in chain order, in place of the import's");
    assert_eq!(a.units[1], echo(42, 0.7));
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
