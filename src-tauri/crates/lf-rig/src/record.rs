//! The rig record as stored: `lf.rig` v1's JSON unchanged (D12), so the page's `upgradeRig` output is
//! `rig_import`'s input byte for byte in meaning. The TypeScript twin is `src/ui/state/rig-record.ts`
//! (`RigRecord`, `RigSlot`, `RigUnit`); `verify/fixtures/rig-record.json` holds both sides to one value
//! (`verify/guards/rig-upgrade.mjs` on the TS side, `tests/fixture.rs` here).
//!
//! The serde derive here is the strict shape: a field of the wrong type, an unknown kind or parameter,
//! a plugin unit without its plugin or a unit id 0 fails the whole parse. The reader the store uses on
//! a file is tolerant (`parseRig`'s rules: a bad unit, an unknown parameter or a reused id skipped, a
//! bad volume read as unity) and lands with the store (CP2a).
//!
//! **Field presence round-trips (D12).** The page reads and writes `bypass` and `params` as optional,
//! so a record it wrote may omit a unit's `bypass` or carry `params: {}`; the native record keeps each
//! as it was written ([`UnitRecord::bypass`], [`UnitBody::Builtin`]'s `params`), never adds or drops
//! a field. Numbers parse exactly (serde_json's `float_roundtrip`): a value the page wrote comes back
//! as the same `f64`.

use std::collections::BTreeMap;

use lf_engine::{UnitId, UnitKind, UnitParam};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::view::PluginRefView;

/// A built-in unit's parameter values, in the app's own shape (`unit_host::Params` in the app crate),
/// stored as a map from each parameter's wire key (`UnitParam::key`) to its value.
pub type Params = Vec<(UnitParam, f64)>;

/// The whole record: `{ version: 1, slots: [slot, slot] }`. Set by the page's import (once, while no
/// record exists) and from then on only by `Rig::commit`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RigRecord {
    pub version: Version1,
    /// Slot A, then slot B.
    pub slots: [SlotRecord; 2],
}

/// The record's version, always 1: a value of this type is a v1 record. Serialized as the number `1`;
/// any other version fails the strict parse (the store reports it as `unreadable` and never writes
/// over it, D8).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Version1;

impl Serialize for Version1 {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u8(1)
    }
}

impl<'de> Deserialize<'de> for Version1 {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match u64::deserialize(d)? {
            1 => Ok(Version1),
            other => Err(serde::de::Error::custom(format!("rig record version {other} is not 1"))),
        }
    }
}

/// One slot's record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SlotRecord {
    /// The slot volume after the chain, 0..=1.5 (`SLOT_GAIN_MAX`), unity at 1. Every durable write
    /// takes it from the settings memory's slot gain when that holds one (the player's level wins),
    /// else keeps the record's (D2).
    pub volume: f64,
    /// The chain in order, head first: each unit the engine applied (until its removal is confirmed)
    /// and each record unit the restore still holds, at its place (D3).
    pub units: Vec<UnitRecord>,
    /// The legacy sends' units still to place (unit contract §6), ids minted and values read; empty
    /// once placed. Stored only when not empty (as `serializeRig` writes it); absent reads empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending: Vec<UnitRecord>,
}

/// One unit as the record keeps it: what an add restores, under its id (unit contract §2 `UnitRecord`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawUnit", into = "RawUnit")]
pub struct UnitRecord {
    /// The unit's id, 1..=u32::MAX, unique across both slots' `units` and `pending`; kept for the
    /// unit's life, across launches (its tone file is `unit-<id>.tone`).
    pub id: UnitId,
    pub body: UnitBody,
    /// Whether the unit is bypassed, as written: `None` when the record omits it, which reads false
    /// ([`UnitRecord::bypassed`], as `addSpec` reads it). Kept apart from `Some(false)` so the
    /// record serializes as it was written.
    pub bypass: Option<bool>,
}

impl UnitRecord {
    /// Whether the unit is bypassed (an omitted `bypass` is not).
    pub fn bypassed(&self) -> bool {
        self.bypass == Some(true)
    }
}

/// What a unit is, by kind.
#[derive(Clone, Debug, PartialEq)]
pub enum UnitBody {
    /// A plugin: its identity, which its tone file is checked against, and the name the scan gave it.
    Plugin(PluginRecord),
    /// A built-in (`kind` is never [`UnitKind::Plugin`]) with the values it was last applied with;
    /// a parameter it does not list starts at the engine's default. `params` is as written: `None`
    /// when the record omits it, `Some` (possibly empty, `{}`) when it carries one.
    Builtin { kind: UnitKind, params: Option<Params> },
}

/// A plugin as the record keeps it (`RigPlugin` in TS): its identity and, for the notice and the logs,
/// its display name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginRecord {
    #[serde(flatten)]
    pub plugin: PluginRefView,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

// ── the wire form of a unit ──────────────────────────────────────────────────────────────────────────

/// The kinds' keys as the wire and `lf.rig` spell them (the app's `chain.rs` `kind_key` table).
const KIND_KEYS: [(UnitKind, &str); 6] = [
    (UnitKind::Plugin, "plugin"),
    (UnitKind::Ott, "ott"),
    (UnitKind::Echo, "echo"),
    (UnitKind::Reverb, "reverb"),
    (UnitKind::Ring, "ring"),
    (UnitKind::Synth, "synth"),
];

/// A unit as JSON spells it: `{ id, kind, plugin?, params?, bypass? }`, each optional field kept
/// present or absent as read.
#[derive(Serialize, Deserialize)]
struct RawUnit {
    id: UnitId,
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plugin: Option<PluginRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    params: Option<BTreeMap<String, f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bypass: Option<bool>,
}

impl TryFrom<RawUnit> for UnitRecord {
    type Error = String;

    fn try_from(raw: RawUnit) -> Result<Self, String> {
        if raw.id == 0 {
            return Err("unit id 0".into());
        }
        let kind = KIND_KEYS.iter().find(|(_, key)| *key == raw.kind).map(|(kind, _)| *kind);
        let body = match (kind, raw.plugin) {
            (None, _) => return Err(format!("unknown unit kind {:?}", raw.kind)),
            (Some(UnitKind::Plugin), Some(plugin)) => UnitBody::Plugin(plugin),
            (Some(UnitKind::Plugin), None) => return Err(format!("plugin unit {} without its plugin", raw.id)),
            (Some(kind), _) => {
                let params = raw
                    .params
                    .map(|map| {
                        map.into_iter()
                            .map(|(key, value)| {
                                let param = UnitParam::from_key(&key).ok_or_else(|| format!("unknown unit parameter {key:?}"))?;
                                Ok((param, value))
                            })
                            .collect::<Result<Params, String>>()
                    })
                    .transpose()?;
                UnitBody::Builtin { kind, params }
            }
        };
        Ok(UnitRecord { id: raw.id, body, bypass: raw.bypass })
    }
}

impl From<UnitRecord> for RawUnit {
    fn from(unit: UnitRecord) -> Self {
        let key = |kind: UnitKind| KIND_KEYS.iter().find(|(k, _)| *k == kind).map(|(_, key)| *key).expect("every unit kind has a key");
        let (kind, plugin, params) = match unit.body {
            UnitBody::Plugin(plugin) => ("plugin", Some(plugin), None),
            UnitBody::Builtin { kind, params } => {
                let params = params.map(|params| params.into_iter().map(|(p, v)| (p.key().to_owned(), v)).collect());
                (key(kind), None, params)
            }
        };
        RawUnit { id: unit.id, kind: kind.to_owned(), plugin, params, bypass: unit.bypass }
    }
}
