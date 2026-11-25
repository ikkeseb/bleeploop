//! The record's shape against the page's: `verify/fixtures/rig-record.json` holds a legacy snapshot
//! (`input`) and the record `upgradeRig` makes of it (`record`; the TS half is
//! `verify/guards/rig-upgrade.mjs`). Here the `record` parses as a [`RigRecord`] and serializes back
//! to the same value (D12: the native record is `lf.rig` v1 unchanged).

use lf_rig::{RigRecord, UnitBody, UnitKind, UnitParam};
use serde_json::Value;

const FIXTURE: &str = include_str!("../../../../verify/fixtures/rig-record.json");

/// The fixture's `record`, as JSON.
fn fixture_record() -> Value {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("the fixture is JSON");
    fixture.get("record").cloned().expect("the fixture has a record")
}

/// `v` with every number as an f64, so `1` and `1.0` (JS writes the first, serde the second for an
/// f64) compare equal; object keys compare in any order.
fn as_values(v: &Value) -> Value {
    match v {
        Value::Number(n) => serde_json::json!(n.as_f64().expect("a finite number")),
        Value::Array(items) => Value::Array(items.iter().map(as_values).collect()),
        Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), as_values(v))).collect()),
        other => other.clone(),
    }
}

#[test]
fn round_trip_holds_the_fixture() {
    let json = fixture_record();
    let record: RigRecord = serde_json::from_value(json.clone()).expect("the fixture's record parses");

    // What it parsed into, so a field read into the wrong place cannot pass as a round trip.
    let [a, b] = &record.slots;
    assert_eq!(a.volume, 0.7);
    assert_eq!(a.units.len(), 1);
    match &a.units[0].body {
        UnitBody::Plugin(p) => {
            assert_eq!((p.plugin.format.as_str(), p.name.as_deref()), ("vst3", Some("Amp Sim")));
        }
        other => panic!("slot A's head is the plugin, got {other:?}"),
    }
    let kinds = |units: &[lf_rig::UnitRecord]| {
        units.iter().map(|u| match &u.body {
            UnitBody::Plugin(_) => UnitKind::Plugin,
            UnitBody::Builtin { kind, .. } => *kind,
        }).collect::<Vec<_>>()
    };
    assert_eq!(kinds(&a.pending), [UnitKind::Ott, UnitKind::Ring, UnitKind::Echo, UnitKind::Reverb]);
    assert_eq!(a.pending.iter().map(|u| (u.id, u.bypass)).collect::<Vec<_>>(), [(2, false), (3, true), (4, false), (5, true)]);
    assert_eq!(kinds(&b.units), [UnitKind::Synth]);
    match &b.units[0].body {
        UnitBody::Builtin { params, .. } => assert!(params.contains(&(UnitParam::Instrument, 2.0))),
        other => panic!("slot B's head is SYNTH, got {other:?}"),
    }
    assert_eq!(kinds(&b.pending), [UnitKind::Ott]);

    let back = serde_json::to_value(&record).expect("the record serializes");
    assert_eq!(as_values(&back), as_values(&json));
    let again: RigRecord = serde_json::from_value(back).expect("its own output parses");
    assert_eq!(again, record);
}
