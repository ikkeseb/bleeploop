//! OWNS: MIDI learn, ported from `src/app/midi-actions.ts`: the persisted binding model, learn capture,
//! matching, momentary versus latching footswitches, and consume-first (a message that matches a
//! binding, or is learned, never reaches the play path).
//!
//! The web keyed a port by its Web MIDI input id; natively a port is its name plus its occurrence (the
//! n-th present port with that name, from 0). A binding the web persisted still deserializes: its `port`
//! id is ignored and a missing `occurrence` reads as 0, the first port with that name.
//!
//! Footswitches (the rule `midi-actions.ts` states in its header): a momentary pedal sends one value on
//! press and the other on release; a latching pedal sends one value per press, alternating; some send
//! the same value every press. The learn gesture tells them apart: a pedal that sends the other side
//! within [`RELEASE`] of its learning press was seen to release, and is momentary: it fires on the
//! press side and swallows the release. Every other binding fires on every message.

use std::time::{Duration, Instant};

use lf_engine::Action;
use serde::{Deserialize, Serialize};

use super::parse::Message;

/// How long after its learning press a release marks a pedal momentary (`RELEASE_MS`).
pub const RELEASE: Duration = Duration::from_millis(1000);

/// The named actions a binding runs (`src/app/actions.ts` `ActionId`), serialized as the TS ids.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ActionId {
    RecDub,
    PlayStop,
    Undo,
    Clear,
    NextTrack,
    PrevTrack,
    PlayAll,
    StopAll,
    /// GO LIVE stays with the plugin host (`lf_engine::Action`): a press reaches the UI as
    /// [`super::MidiEvent::GoLive`].
    GoLive,
}

impl ActionId {
    /// The engine's action, or `None` for GO LIVE.
    pub fn engine_action(self) -> Option<Action> {
        Some(match self {
            ActionId::RecDub => Action::RecDub,
            ActionId::PlayStop => Action::PlayStop,
            ActionId::Undo => Action::Undo,
            ActionId::Clear => Action::Clear,
            ActionId::NextTrack => Action::NextTrack,
            ActionId::PrevTrack => Action::PrevTrack,
            ActionId::PlayAll => Action::PlayAll,
            ActionId::StopAll => Action::StopAll,
            ActionId::GoLive => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Cc,
    Note,
}

/// A port as bindings name it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PortKey {
    pub name: String,
    /// Among the present ports with this name, which one (0 = the first in the system's port list).
    pub occurrence: u32,
}

/// One learned message (`MidiBinding`). `press_high`: the learning press sent a CC value ≥ 64, or a
/// note-on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Binding {
    pub port_name: String,
    #[serde(default)]
    pub occurrence: u32,
    pub channel: u8,
    pub kind: Kind,
    pub number: u8,
    pub action: ActionId,
    pub press_high: bool,
    pub momentary: bool,
}

impl Binding {
    fn matches(&self, port: &PortKey, channel: u8, kind: Kind, number: u8) -> bool {
        self.port_name == port.name && self.occurrence == port.occurrence && self.channel == channel && self.kind == kind && self.number == number
    }

    /// `isBinding`'s ranges (the types serde checks).
    fn is_valid(&self) -> bool {
        self.channel < 16 && self.number < 128
    }
}

/// The persisted bindings (a JSON array, as `load()` reads localStorage): anything malformed, or naming
/// an action that no longer exists, is dropped one entry at a time.
pub fn parse_bindings(json: &str) -> Vec<Binding> {
    let Ok(list) = serde_json::from_str::<Vec<serde_json::Value>>(json) else { return Vec::new() };
    list.into_iter().filter_map(|v| serde_json::from_value::<Binding>(v).ok()).filter(Binding::is_valid).collect()
}

/// What one message did to MIDI learn.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Outcome {
    /// The message is MIDI learn's: the play path never sees it.
    pub consumed: bool,
    /// The action to run now.
    pub fire: Option<ActionId>,
    /// The binding this message just learned.
    pub learned: Option<Binding>,
    /// The binding list changed (a learn, or a pedal read as momentary): persist it.
    pub changed: bool,
}

/// `midi-actions.ts` module state: the bindings, the pending learn, and the learn's tail.
#[derive(Default)]
pub(crate) struct Learn {
    pub(crate) bindings: Vec<Binding>,
    pub(crate) learning: Option<ActionId>,
    /// The binding just learned, and until when its learning press may still be followed by a release.
    tail: Option<(Binding, Instant)>,
}

impl Learn {
    /// Replace the bindings (settings handed over, or one forgotten). A learn tail survives while its
    /// binding does (`forget` drops the tail only with its own binding).
    pub(crate) fn set_bindings(&mut self, bindings: Vec<Binding>) {
        if self.tail.as_ref().is_some_and(|(b, _)| !bindings.contains(b)) {
            self.tail = None;
        }
        self.bindings = bindings;
    }

    /// `consume`: learn capture first, then the bound message. `at` is the message's arrival.
    pub(crate) fn consume(&mut self, port: &PortKey, message: &Message, at: Instant) -> Outcome {
        let (kind, number, high) = match *message {
            Message::Cc { controller, value, .. } => (Kind::Cc, controller, value >= 64),
            Message::NoteOn { note, .. } => (Kind::Note, note, true),
            Message::NoteOff { note, .. } => (Kind::Note, note, false),
            Message::PitchBend { .. } => return Outcome::default(),
        };
        let channel = message.channel();

        // CC 120–127 are channel-mode messages (all sound off, all notes off, …), never a switch; a
        // note-off never starts a learn.
        if let Some(action) = self.learning {
            if if kind == Kind::Cc { number < 120 } else { high } {
                let binding = Binding {
                    port_name: port.name.clone(),
                    occurrence: port.occurrence,
                    channel,
                    kind,
                    number,
                    action,
                    press_high: high,
                    momentary: false,
                };
                // One action per message: learning a bound message again moves it.
                self.bindings.retain(|b| !b.matches(port, channel, kind, number));
                self.bindings.push(binding.clone());
                self.tail = Some((binding.clone(), at + RELEASE));
                self.learning = None;
                return Outcome { consumed: true, learned: Some(binding), changed: true, ..Outcome::default() };
            }
        }

        let Some(i) = self.bindings.iter().position(|b| b.matches(port, channel, kind, number)) else {
            return Outcome::default();
        };
        if let Some((tail, until)) = self.tail.take_if(|(b, _)| *b == self.bindings[i]) {
            if high != tail.press_high && at < until {
                self.bindings[i].momentary = true;
                return Outcome { consumed: true, changed: true, ..Outcome::default() };
            }
        }
        let b = &self.bindings[i];
        let fire = (!b.momentary || high == b.press_high).then_some(b.action);
        Outcome { consumed: true, fire, ..Outcome::default() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str, occurrence: u32) -> PortKey {
        PortKey { name: name.into(), occurrence }
    }

    fn cc(channel: u8, controller: u8, value: u8) -> Message {
        Message::Cc { channel, controller, value }
    }

    // midi-actions.ts isBinding/load: the persisted list, each malformed entry dropped on its own. The
    // web's `port` id is ignored, and a binding without `occurrence` names the first port of its name.
    #[test]
    fn the_web_persisted_list_deserializes_and_malformed_entries_are_dropped() {
        let json = r#"[
            {"port":"input-3","portName":"Pedal","channel":0,"kind":"cc","number":20,"action":"recDub","pressHigh":true,"momentary":true},
            {"portName":"Keys","occurrence":1,"channel":2,"kind":"note","number":60,"action":"goLive","pressHigh":true,"momentary":false},
            {"port":"x","portName":"Pedal","channel":16,"kind":"cc","number":20,"action":"recDub","pressHigh":true,"momentary":true},
            {"port":"x","portName":"Pedal","channel":0,"kind":"cc","number":128,"action":"recDub","pressHigh":true,"momentary":true},
            {"port":"x","portName":"Pedal","channel":0,"kind":"pc","number":1,"action":"recDub","pressHigh":true,"momentary":true},
            {"port":"x","portName":"Pedal","channel":0,"kind":"cc","number":1,"action":"selfDestruct","pressHigh":true,"momentary":true},
            {"port":"x","portName":"Pedal","channel":1.5,"kind":"cc","number":1,"action":"undo","pressHigh":true,"momentary":true},
            {"port":"x","channel":0,"kind":"cc","number":1,"action":"undo","pressHigh":true,"momentary":true},
            "junk", null
        ]"#;
        let list = parse_bindings(json);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0], Binding {
            port_name: "Pedal".into(),
            occurrence: 0,
            channel: 0,
            kind: Kind::Cc,
            number: 20,
            action: ActionId::RecDub,
            press_high: true,
            momentary: true,
        });
        assert_eq!((list[1].occurrence, list[1].action, list[1].kind), (1, ActionId::GoLive, Kind::Note));
        assert!(parse_bindings("not json").is_empty());
        assert!(parse_bindings("{}").is_empty());
        // Round trip, in the web's field names.
        let back = serde_json::to_string(&list[0]).unwrap();
        assert_eq!(back, r#"{"portName":"Pedal","occurrence":0,"channel":0,"kind":"cc","number":20,"action":"recDub","pressHigh":true,"momentary":true}"#);
        assert_eq!(parse_bindings(&format!("[{back}]")), [list[0].clone()]);
    }

    // actions.ts: every ActionId but goLive is an engine action.
    #[test]
    fn every_action_but_go_live_is_an_engine_action() {
        assert_eq!(ActionId::NextTrack.engine_action(), Some(Action::NextTrack));
        assert_eq!(ActionId::Clear.engine_action(), Some(Action::Clear));
        assert_eq!(ActionId::GoLive.engine_action(), None);
    }

    // midi-actions.ts consume: a binding matches on port, channel, kind and number, nothing less; natively
    // the port is its name and occurrence.
    #[test]
    fn a_binding_matches_its_port_occurrence_channel_kind_and_number_only() {
        let mut l = Learn::default();
        let t = Instant::now();
        l.learning = Some(ActionId::Undo);
        l.consume(&key("Pedal", 1), &cc(2, 20, 127), t);
        let later = t + RELEASE * 2;
        for (port, m) in [
            (key("Pedal", 0), cc(2, 20, 127)),
            (key("Other", 1), cc(2, 20, 127)),
            (key("Pedal", 1), cc(3, 20, 127)),
            (key("Pedal", 1), cc(2, 21, 127)),
            (key("Pedal", 1), Message::NoteOn { channel: 2, note: 20, velocity: 100 }),
        ] {
            assert_eq!(l.consume(&port, &m, later), Outcome::default(), "{port:?} {m:?}");
        }
        assert_eq!(l.consume(&key("Pedal", 1), &cc(2, 20, 127), later).fire, Some(ActionId::Undo));
    }

    // midi-actions.ts consume: "One action per message: learning a bound message again moves it".
    #[test]
    fn learning_a_bound_message_again_moves_it() {
        let mut l = Learn::default();
        let t = Instant::now();
        l.learning = Some(ActionId::Undo);
        l.consume(&key("P", 0), &cc(0, 20, 127), t);
        l.learning = Some(ActionId::PlayAll);
        let out = l.consume(&key("P", 0), &cc(0, 20, 127), t + RELEASE * 2);
        assert!(out.changed && out.fire.is_none());
        assert_eq!(l.bindings.len(), 1);
        assert_eq!(l.bindings[0].action, ActionId::PlayAll);
    }

    // midi-actions.ts forget: dropping the binding just learned drops its tail, so relearning it later
    // cannot read an old release; a list that keeps it keeps the tail.
    #[test]
    fn a_learn_tail_lives_as_long_as_its_binding() {
        let mut l = Learn::default();
        let t = Instant::now();
        l.learning = Some(ActionId::Undo);
        l.consume(&key("P", 0), &cc(0, 20, 127), t);
        l.set_bindings(l.bindings.clone());
        assert!(l.tail.is_some(), "the same list handed back keeps the tail");
        l.set_bindings(Vec::new());
        assert!(l.tail.is_none());
    }
}
