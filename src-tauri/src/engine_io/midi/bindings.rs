//! OWNS: the MIDI-learn binding model, ported from `src/app/midi-actions.ts` (`MidiBinding`,
//! `fromStored`) and `src/app/actions.ts` (`ActionId`, `Target`, `isLaneAction`): what a binding names,
//! the invariants every binding keeps, and the fallible parse of a persisted list. What a message does to
//! the bindings (capture, matching, the footswitch read, HOLD) is `super::learn`.
//!
//! A binding names its port by `port_id`, an opaque identity string matched by equality, and keeps the
//! port's name for display only. Which identity a port gets (the device-interface path plus a
//! discriminator, or a legacy record's name) is the store's and the port table's (port identity, the
//! import never guesses: `docs/ARCHITECTURE.md` § Decided: native MIDI), never this file's.

use lf_engine::TRACK_COUNT;
use serde::{Deserialize, Serialize};

/// The named actions a binding runs (`actions.ts` `ActionId`), serialized as the TS ids. The variant order
/// is `ACTION_LABELS`'s (the learn picker's): the lane actions first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ActionId {
    RecDub,
    PlayStop,
    Undo,
    Clear,
    Mute,
    Reverse,
    Copy,
    HalveTrack,
    NextTrack,
    PrevTrack,
    PlayAll,
    StopAll,
    FadeAll,
    GoLive,
    StageView,
    StageNextView,
    TapTempo,
    ClickToggle,
    EndStopToggle,
    FixedToggle,
    RetakeToggle,
    AutoRecToggle,
    InFxEcho,
    InFxReverb,
    InFxRing,
}

impl ActionId {
    /// Every action, in `ACTION_LABELS` order.
    pub const ALL: [ActionId; 25] = [
        ActionId::RecDub,
        ActionId::PlayStop,
        ActionId::Undo,
        ActionId::Clear,
        ActionId::Mute,
        ActionId::Reverse,
        ActionId::Copy,
        ActionId::HalveTrack,
        ActionId::NextTrack,
        ActionId::PrevTrack,
        ActionId::PlayAll,
        ActionId::StopAll,
        ActionId::FadeAll,
        ActionId::GoLive,
        ActionId::StageView,
        ActionId::StageNextView,
        ActionId::TapTempo,
        ActionId::ClickToggle,
        ActionId::EndStopToggle,
        ActionId::FixedToggle,
        ActionId::RetakeToggle,
        ActionId::AutoRecToggle,
        ActionId::InFxEcho,
        ActionId::InFxReverb,
        ActionId::InFxRing,
    ];

    /// A lane action acts on a [`Target`] (`isLaneAction`); a global one has none.
    pub fn is_lane(self) -> bool {
        matches!(
            self,
            ActionId::RecDub
                | ActionId::PlayStop
                | ActionId::Undo
                | ActionId::Clear
                | ActionId::Mute
                | ActionId::Reverse
                | ActionId::Copy
                | ActionId::HalveTrack
        )
    }
}

/// A lane action's track, 0-based below `TRACK_COUNT`; `None` is the selected track. Always `None` for a
/// global action.
pub type Target = Option<u8>;

/// `target` as a binding for `action` keeps it: a named track for a lane action only, and only a track
/// that exists.
pub(crate) fn target_for(action: ActionId, target: Target) -> Target {
    target.filter(|&t| action.is_lane() && usize::from(t) < TRACK_COUNT)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Cc,
    Note,
}

/// One learned message (`MidiBinding`). `press_high`: the learning press sent a CC value ≥ 64, or a
/// note-on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Binding {
    /// The port's identity, matched by equality. A record the web persisted names it `port` (its Web MIDI
    /// input id, kept as it was: the migration decides what it maps to).
    #[serde(alias = "port")]
    pub port_id: String,
    pub port_name: String,
    pub channel: u8,
    pub kind: Kind,
    pub number: u8,
    pub action: ActionId,
    /// See [`Target`]; absent in a record saved before targets.
    #[serde(default)]
    pub target: Target,
    pub press_high: bool,
    pub momentary: bool,
    /// HOLD, on a momentary REC/DUB pedal only: its release ends the capture its press started. Absent in
    /// a record saved before HOLD.
    #[serde(default)]
    pub hold: bool,
}

impl Binding {
    /// The binding with its invariants kept (`fromStored`): a target only on a lane action, HOLD only on a
    /// momentary REC/DUB.
    pub(crate) fn normalized(self) -> Binding {
        Binding {
            target: target_for(self.action, self.target),
            hold: self.hold && self.momentary && self.action == ActionId::RecDub,
            ..self
        }
    }
}

/// A persisted record [`parse_bindings`] could not read, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Rejected {
    /// Its position in the persisted array.
    pub index: usize,
    pub reason: String,
}

/// What [`parse_bindings`] read: the bindings, in their persisted order, and every record it refused.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Parsed {
    pub bindings: Vec<Binding>,
    pub rejected: Vec<Rejected>,
}

/// One persisted record as `fromStored` reads it, except that what it would drop is an error: a
/// malformed field, an action that no longer exists, a channel, number or named track out of range.
/// A record saved before targets or HOLD reads with neither; a target on a global action, or HOLD where
/// it cannot apply, is dropped from the record as `fromStored` drops it.
pub fn parse_binding(value: serde_json::Value) -> Result<Binding, String> {
    let b: Binding = serde_json::from_value(value).map_err(|e| e.to_string())?;
    if b.channel >= 16 {
        return Err(format!("channel {} is out of range", b.channel));
    }
    if b.number >= 128 {
        return Err(format!("number {} is out of range", b.number));
    }
    if let Some(t) = b.target.filter(|&t| usize::from(t) >= TRACK_COUNT) {
        return Err(format!("track {t} does not exist"));
    }
    Ok(b.normalized())
}

/// The persisted bindings (a JSON array, as `load()` reads localStorage). As the store does, this
/// reports, never silently empties, so a document that is not an array is an error and each
/// unreadable record is reported in [`Parsed::rejected`] beside the ones that read.
pub fn parse_bindings(json: &str) -> Result<Parsed, String> {
    let list: Vec<serde_json::Value> = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let mut parsed = Parsed::default();
    for (index, value) in list.into_iter().enumerate() {
        match parse_binding(value) {
            Ok(b) => parsed.bindings.push(b),
            Err(reason) => parsed.rejected.push(Rejected { index, reason }),
        }
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    // actions.ts ActionId and ACTION_LABELS: the 25 ids, in the picker's order, the first eight lanes.
    #[test]
    fn the_25_actions_serialize_as_the_ts_ids_in_label_order_and_the_first_eight_are_lanes() {
        let ids = [
            "recDub", "playStop", "undo", "clear", "mute", "reverse", "copy", "halveTrack", "nextTrack",
            "prevTrack", "playAll", "stopAll", "fadeAll", "goLive", "stageView", "stageNextView", "tapTempo",
            "clickToggle", "endStopToggle", "fixedToggle", "retakeToggle", "autoRecToggle", "inFxEcho",
            "inFxReverb", "inFxRing",
        ];
        for (action, id) in ActionId::ALL.into_iter().zip(ids) {
            assert_eq!(serde_json::to_value(action).unwrap(), id);
            assert_eq!(serde_json::from_value::<ActionId>(id.into()).unwrap(), action);
        }
        let lanes: Vec<bool> = ActionId::ALL.iter().map(|a| a.is_lane()).collect();
        assert_eq!(lanes, [[true; 8].as_slice(), &[false; 17]].concat());
    }

    // midi-actions.ts fromStored: a record saved before targets and HOLD reads with neither; a target is
    // kept for a lane action only, HOLD for a momentary recDub only. The web's `port` reads as `portId`.
    #[test]
    fn a_stored_record_keeps_a_target_for_a_lane_action_and_hold_for_a_momentary_rec_dub_only() {
        let rec = |extra: &str| {
            format!(r#"{{"port":"input-3","portName":"Pedal","channel":0,"kind":"cc","number":20,"pressHigh":true,{extra}}}"#)
        };
        let json = format!(
            "[{},{},{},{},{}]",
            rec(r#""action":"recDub","momentary":true"#),
            rec(r#""action":"recDub","momentary":true,"target":2,"hold":true"#),
            rec(r#""action":"playAll","momentary":true,"target":2,"hold":true"#),
            rec(r#""action":"recDub","momentary":false,"target":null,"hold":true"#),
            rec(r#""action":"undo","momentary":true,"target":4,"hold":true"#),
        );
        let p = parse_bindings(&json).unwrap();
        assert_eq!(p.rejected, []);
        let got: Vec<(ActionId, Target, bool)> = p.bindings.iter().map(|b| (b.action, b.target, b.hold)).collect();
        assert_eq!(
            got,
            [
                (ActionId::RecDub, None, false),
                (ActionId::RecDub, Some(2), true),
                (ActionId::PlayAll, None, false),
                (ActionId::RecDub, None, false),
                (ActionId::Undo, Some(4), false),
            ]
        );
        assert_eq!((p.bindings[0].port_id.as_str(), p.bindings[0].port_name.as_str()), ("input-3", "Pedal"));
        // Round trip, in the native field names.
        let back = serde_json::to_string(&p.bindings[1]).unwrap();
        assert_eq!(
            back,
            r#"{"portId":"input-3","portName":"Pedal","channel":0,"kind":"cc","number":20,"action":"recDub","target":2,"pressHigh":true,"momentary":true,"hold":true}"#
        );
        assert_eq!(parse_bindings(&format!("[{back}]")).unwrap().bindings, [p.bindings[1].clone()]);
    }

    // A record fromStored would drop is reported by its index, beside the records
    // that read; a document that is not an array is an error, never an empty list.
    #[test]
    fn an_unreadable_record_is_reported_by_index_and_an_unreadable_document_is_an_error() {
        let ok = r#"{"portId":"x","portName":"P","channel":0,"kind":"cc","number":1,"action":"undo","pressHigh":true,"momentary":true}"#;
        let json = format!(
            "[{ok},{},{},{},{},{},{},{},{},\"junk\",null,{ok}]",
            ok.replace(r#""action":"undo""#, r#""action":"selfDestruct""#),
            ok.replace(r#""channel":0"#, r#""channel":16"#),
            ok.replace(r#""number":1"#, r#""number":128"#),
            ok.replace(r#""kind":"cc""#, r#""kind":"pc""#),
            ok.replace(r#""channel":0"#, r#""channel":1.5"#),
            ok.replace(r#""portName":"P","#, ""),
            ok.replace(r#""momentary":true"#, r#""momentary":true,"target":5"#),
            ok.replace(r#""momentary":true"#, r#""momentary":true,"hold":"yes""#),
        );
        let p = parse_bindings(&json).unwrap();
        assert_eq!(p.bindings.len(), 2);
        assert_eq!(p.rejected.iter().map(|r| r.index).collect::<Vec<_>>(), (1..=10).collect::<Vec<_>>());
        assert!(p.rejected[0].reason.contains("selfDestruct"), "{}", p.rejected[0].reason);
        assert!(parse_bindings("not json").is_err());
        assert!(parse_bindings("{}").is_err());
        assert_eq!(parse_bindings("[]"), Ok(Parsed::default()));
    }
}
