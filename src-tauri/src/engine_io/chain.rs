//! OWNS: the model of the slots' chains (E2, `docs/unit-contract.md` §2): each slot's plates as the
//! engine applied them, the pending ops per slot, the revision the feed tells the UI, and the wire's
//! shapes of it ([`ChainState`], [`UnitState`], [`UnitSpec`]). The UI draws the chain from the feed's
//! `chains` alone (invariant 3). The table and the handles are `unit_host.rs`.

// The seam (E2 S0): the bodies are stubs that later pieces fill, so nothing here is called yet.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use lf_engine::grid::Frame;
use lf_engine::{Event, UnitId, UnitKind, UnitParam, SLOT_COUNT};
use serde::de::{Error as _, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::unit_host::{Params, Spec};

/// A plugin's identity, as the scan names it (`format`, `path`, `id`): the tone store's identity, so a
/// unit's plugin and its tone are one key.
pub(crate) type PluginRef = crate::host::tone::ToneIdentity;

/// The model: per slot, its plates in plate order and its pending ops, and the revision `chains`
/// carries. P2 builds it into `Core`.
///
/// - Lock order: ports → settings → ends → model; nothing is taken under the model. `drain` (`mod.rs`)
///   gains the model and projects `Seat`, `Chain` and `UnitParam` there.
/// - Per slot the pending ops are kept in push order; a `Seat` pops the head (`Waiting` marks it held,
///   its second answer settles it). A `Seat` whose unit or op differs from the head is a counted
///   protocol error, logged.
/// - From `begin_rebuild` on it ignores the old generation's events (its eviction `Chain` included).
/// - Legacy rows are in its index arithmetic and never in `chains`.
pub(crate) struct Model {}

/// The model's snapshot for the feed: `FeedDrain` carries it, read in the same `settings` + `ends`
/// acquisition as the events.
#[derive(Clone, Debug)]
pub struct ChainRev {
    pub rev: u64,
    pub chains: Arc<[ChainState; SLOT_COUNT]>,
}

/// The engine events a wait drained for itself while the feed was stale (`EngineHost::wait_op`), for
/// the feed to send. Tagged by generation and cleared at a swap; filled only when no drain ran for
/// [`STASH_AFTER`] (so never beside a ticking feed). Every event is projected into settings and the
/// model as it leaves the ring, once. Past [`STASH_LEN`] it drops the oldest and latches `lost` (the
/// feed sends a reset).
pub(crate) struct Stash {
    pub(crate) generation: u64,
    pub(crate) events: VecDeque<Event>,
    pub(crate) lost: bool,
}

/// [`Stash`]'s bound (`Core::stash`).
pub(crate) const STASH_LEN: usize = 4096;
/// How long without a feed drain before a wait drains for itself.
pub(crate) const STASH_AFTER: Duration = Duration::from_millis(250);

/// One slot's chain as applied (the feed's `chains`, on a reset frame and when the revision moved):
/// its units in order, their latencies' sum (bypassed ones included), and whether an edit waits for a
/// take to end.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainState {
    pub units: Vec<UnitState>,
    pub latency: Frame,
    pub held: bool,
}

/// A plate's state on the wire (lowercase).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlateState {
    Loading,
    Waiting,
    Seated,
    Leaving,
    Failed,
}

/// A unit as the engine applied it. `plugin` for a plugin, `params` for a built-in; the flags are
/// present only when true.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnitState {
    pub id: UnitId,
    #[serde(with = "kind_key")]
    pub kind: UnitKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<PluginRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<WireParams>,
    pub bypass: bool,
    pub source: bool,
    pub name: String,
    pub latency: Frame,
    pub state: PlateState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// `failed` by a park: withdrawn, its seat kept with its place and latency.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub parked: bool,
    /// An edit of this unit waits for a take to end.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub held: bool,
    /// `loading` or `waiting` by a reload: the old instance stays seated until the new one lands.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reloading: bool,
}

/// What restores a unit (`chain_add`'s `spec`, the TS `UnitSpec`): `{kind:"plugin", plugin}` or a
/// built-in's `{kind, params?}`, each with an optional `bypass`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnitSpec {
    #[serde(with = "kind_key")]
    pub kind: UnitKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<PluginRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<WireParams>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bypass: bool,
}

impl UnitSpec {
    /// The host's [`Spec`] and the bypass it asks for; Err for a plugin without its `plugin`, or a
    /// built-in with one.
    pub fn into_spec(self) -> Result<(Spec, bool), String> {
        todo!("E2 P4")
    }
}

/// A built-in's params on the wire: an object of their keys (`UnitParam::key`), in order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WireParams(pub Params);

impl Serialize for WireParams {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(self.0.len()))?;
        for (param, value) in &self.0 {
            map.serialize_entry(param.key(), value)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for WireParams {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Keys;
        impl<'de> Visitor<'de> for Keys {
            type Value = WireParams;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an object of unit parameter keys and numbers")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<WireParams, A::Error> {
                let mut params = Params::new();
                while let Some((key, value)) = map.next_entry::<String, f64>()? {
                    let param = UnitParam::from_key(&key).ok_or_else(|| A::Error::custom(format!("unknown unit parameter {key:?}")))?;
                    params.push((param, value));
                }
                Ok(WireParams(params))
            }
        }
        d.deserialize_map(Keys)
    }
}

/// A [`UnitKind`] as its wire key: `plugin`, `ott`, `echo`, `reverb`, `ring`, `synth`.
mod kind_key {
    use lf_engine::UnitKind;
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serializer};

    const KEYS: [(UnitKind, &str); 6] = [
        (UnitKind::Plugin, "plugin"),
        (UnitKind::Ott, "ott"),
        (UnitKind::Echo, "echo"),
        (UnitKind::Reverb, "reverb"),
        (UnitKind::Ring, "ring"),
        (UnitKind::Synth, "synth"),
    ];

    pub(super) fn serialize<S: Serializer>(kind: &UnitKind, s: S) -> Result<S::Ok, S::Error> {
        let key = KEYS.iter().find(|(k, _)| k == kind).map(|(_, key)| *key).expect("every unit kind has a key");
        s.serialize_str(key)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<UnitKind, D::Error> {
        let key = String::deserialize(d)?;
        KEYS.iter().find(|(_, k)| *k == key).map(|(kind, _)| *kind).ok_or_else(|| D::Error::custom(format!("unknown unit kind {key:?}")))
    }
}
