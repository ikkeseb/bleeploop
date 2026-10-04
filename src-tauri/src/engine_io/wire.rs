//! OWNS: the JSON wire between the UI and the engine host: the serde
//! mirror of lf-engine's commands and events (the crate stays serde-free), the feed
//! frame the feed thread sends, and the fixture test. The TS mirror is `src/platform/engine-wire.ts`;
//! `verify/fixtures/engine-wire.json` holds both sides to the same JSON (this file's test and a TS
//! guard parse it).
//!
//! Serde's external tagging with the Rust variant names: a unit variant is its name (`"PlayAll"`), a
//! newtype `{"RecDub":0}`, a tuple `{"SetVolume":[0,0.8]}`, a struct variant an object with camelCase
//! fields. `FxParam`/`FxKind` travel as the TS keys (`src/ui/state/fx-metadata.ts`), an `InputSend` and an
//! `InputSendParam` as their `key()`, an `Instrument` as its id, a `NoteTarget` as `{"Builtin":"lead"}` /
//! `{"Slot":0}` / `"Off"`, a `Frame` (i64) as a JSON number. The
//! mirrors are serde `remote` derives: a variant or field lf-engine adds fails to compile here until it
//! is mirrored.
//!
//! A lane's mix ([`WireLaneMix`]) travels in session.json's track shape, inside the session bytes'
//! header (`session.rs`): a snapshot's track as the engine applied it, a load's (required) to set; the
//! fixture's `snapshotHeaders` and `loadHeaders` hold them.
//!
//! `DeviceRequest`, `DeviceStatus`, `DeviceEvent`, `OpenError` and `AudioBackend` derive serde where they
//! are defined (camelCase fields, backends as `"Asio"` / `"Wasapi"`; a request's `inputChannels` is one
//! pick per slot, and one `inputChannel` instead sets both; a request without `sampleRate` asks for the
//! device's own rate; an `OpenError` is its text, or
//! `{"RateChange":{…}}` for a refusal).
//!
//! The commands that carry it are `mode.rs`'s `engine_*`: `engine_send` is a synchronous batch (IPC
//! order holds); `engine_feed` subscribes a Tauri `Channel`, one subscriber at a time (a new one
//! replaces it), and its first frame is a `reset`.

use std::collections::BTreeMap;

use lf_engine::dsp::fx::{FxKind, FxParam, FxState, MAX_PARAMS};
use lf_engine::grid::Frame;
use lf_engine::{Action, Command, Event, InputSend, InputSendParam, Instrument, LaneInfo, LaneMix, LaneState, NoteTarget, Refusal};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{DeviceEvent, DeviceStatus};

/// A command as it crosses `engine_send`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WireCommand(#[serde(with = "CommandDef")] pub Command);

/// An engine event as the feed carries it.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WireEvent(#[serde(with = "EventDef")] pub Event);

#[derive(Serialize, Deserialize)]
#[serde(remote = "Command")]
enum CommandDef {
    RecDub(u8),
    PlayStop(u8),
    Stop(u8),
    Undo(u8),
    Reverse(u8),
    Copy(u8),
    Trim(u8, u32),
    Clear(u8),
    PlayAll,
    StopAll,
    ClearAll,
    Action(#[serde(with = "ActionDef")] Action),
    ActionOn(u8, #[serde(with = "ActionDef")] Action),
    Press,
    SelectTrack(u8),
    SetBpm(f64),
    SetMetronome(bool),
    SetClickVolume(f32),
    SetMasterVolume(f32),
    SetMasterMute(bool),
    SetLoopEndStop(bool),
    SetFadeBars(u32),
    SetFixedLength(bool),
    SetFixedBars(f64),
    SetRetake(bool),
    SetAutoRecord(bool),
    SetAutoSensitivity(f64),
    SetVolume(u8, f32),
    SetMute(u8, bool),
    SetDubFeedback(u8, f32),
    SetFxParam(u8, #[serde(with = "fx_param")] FxParam, f64),
    SetFxBypass(u8, #[serde(with = "fx_kind")] FxKind, bool),
    SelectInstrument(#[serde(with = "NoteTargetDef")] NoteTarget),
    NoteOn(u8, f32),
    NoteOff(u8),
    PitchBend(f64),
    Modulation(f64),
    AllNotesOff,
    SetSlotLive(u8, bool),
    SetSlotGain(u8, f32),
    SetInstrumentGain(#[serde(with = "instrument")] Instrument, f32),
    SetInputSend(#[serde(with = "input_send")] InputSend, bool),
    SetInputSendParam(#[serde(with = "input_send_param")] InputSendParam, f64),
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "Action")]
enum ActionDef {
    RecDub,
    PlayStop,
    Undo,
    Clear,
    NextTrack,
    PrevTrack,
    PlayAll,
    StopAll,
    Mute,
    Reverse,
    Copy,
    Halve,
    Hold(u8),
    Release(u8),
    FadeAll,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "NoteTarget")]
enum NoteTargetDef {
    Builtin(#[serde(with = "instrument")] Instrument),
    Slot(u8),
    Off,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "Event", rename_all_fields = "camelCase")]
enum EventDef {
    Lane {
        frame: Frame,
        lane: u8,
        #[serde(with = "LaneInfoDef")]
        info: LaneInfo,
    },
    Transport { frame: Frame, master: Frame, bpm: u32, locked: bool },
    Beat { frame: Frame, beat_in_bar: u8, count_left: u8, clicked: bool },
    Selected { frame: Frame, lane: u8 },
    Refused {
        frame: Frame,
        lane: u8,
        #[serde(with = "RefusalDef")]
        reason: Refusal,
    },
    TakeRejected { frame: Frame, lane: u8, overdub: bool },
    PassDropped { frame: Frame, lane: u8, pass: u32 },
    Copied { frame: Frame, from: u8, to: u8, feedback: f32 },
    Cleared { frame: Frame, lane: u8 },
    Muted { frame: Frame, lane: u8, on: bool },
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "LaneInfo", rename_all = "camelCase")]
struct LaneInfoDef {
    #[serde(with = "LaneStateDef")]
    state: LaneState,
    length: Frame,
    armed: bool,
    auto_armed: bool,
    can_undo: bool,
    can_reverse: bool,
    reversed: bool,
    stop_at: Option<Frame>,
    fading: bool,
    retake_pass: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "LaneState")]
enum LaneStateDef {
    Empty,
    Recording,
    Overdubbing,
    Playing,
    Stopped,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "Refusal")]
enum RefusalDef {
    Stopping,
    PlayFirst,
    Reversed,
    OtherRecording,
    Empty,
    NoUndo,
    NoClear,
    ConfirmClear,
    Capturing,
    NoTrim,
    NoMute,
    NoReverse,
    NoCopy,
    NoFreeLane,
    Fading,
    NoFade,
}

/// An `Instrument` as its id (`Instrument::id`).
mod instrument {
    use super::*;

    pub fn serialize<S: Serializer>(i: &Instrument, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(i.id())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Instrument, D::Error> {
        let id = String::deserialize(d)?;
        Instrument::from_id(&id).ok_or_else(|| serde::de::Error::custom(format!("unknown instrument \"{id}\"")))
    }
}

/// The TS `FxKind` keys, in `FxKind::ALL` order (`FX_META` in `src/ui/state/fx-metadata.ts`).
const FX_KIND_KEYS: [&str; 5] = ["filter", "pitch", "stutter", "delay", "reverb"];

/// An `FxKind` as its TS key.
mod fx_kind {
    use super::*;

    pub fn serialize<S: Serializer>(k: &FxKind, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(FX_KIND_KEYS[k.index()])
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<FxKind, D::Error> {
        let key = String::deserialize(d)?;
        let i = FX_KIND_KEYS.iter().position(|k| *k == key).ok_or_else(|| serde::de::Error::custom(format!("unknown FX kind \"{key}\"")))?;
        Ok(FxKind::ALL[i])
    }
}

/// An `FxParam` as its def's key (the keys are unique across the kinds).
mod fx_param {
    use super::*;

    pub fn serialize<S: Serializer>(p: &FxParam, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(p.def().key)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<FxParam, D::Error> {
        let key = String::deserialize(d)?;
        FxKind::ALL
            .into_iter()
            .find_map(|kind| FxParam::from_key(kind, &key))
            .ok_or_else(|| serde::de::Error::custom(format!("unknown FX param \"{key}\"")))
    }
}

/// An `InputSend` as its key (`InputSend::key`).
mod input_send {
    use super::*;

    pub fn serialize<S: Serializer>(v: &InputSend, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(v.key())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<InputSend, D::Error> {
        let key = String::deserialize(d)?;
        InputSend::from_key(&key).ok_or_else(|| serde::de::Error::custom(format!("unknown input send \"{key}\"")))
    }
}

/// An `InputSendParam` as its key (`InputSendParam::key`).
mod input_send_param {
    use super::*;

    pub fn serialize<S: Serializer>(v: &InputSendParam, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(v.key())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<InputSendParam, D::Error> {
        let key = String::deserialize(d)?;
        InputSendParam::from_key(&key).ok_or_else(|| serde::de::Error::custom(format!("unknown input send param \"{key}\"")))
    }
}

/// Where the feed's playhead comes from: device frame `frame` was rendered at Unix time `at_ms`, the
/// device runs `rate` frames a second, and loop position 0 plays at `grid + k * master`
/// (`Looper::anchor`): at device frame `f` a lane plays `(f - grid) mod master`. `at_ms` is when the
/// callback rendering `frame` entered; the player hears it the output latency later
/// (`DeviceStatus::align_frames - input_frames`).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClockAnchor {
    pub frame: Frame,
    pub at_ms: f64,
    pub rate: u32,
    pub grid: Frame,
}

/// The device input (the capture channels the slots take, before any plugin) since the last frame: the
/// louder one's linear peak, and whether a sample reached full scale.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Meter {
    pub peak: f32,
    pub clip: bool,
}

/// Frames per waveform bin (the web looper's `PEAK_FRAMES`).
pub const PEAK_BIN_FRAMES: usize = lf_engine::overview::PEAK_FRAMES;

/// Changed waveform bins of one lane: bins `start..start + min.len()` of `PEAK_BIN_FRAMES` frames, in
/// the order the lane plays them (a reversed lane's bins reversed, to within one bin). `count` is the
/// lane's bin total after this update: the frames it recorded so far, or its loop (0: empty).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PeakUpdate {
    pub lane: u8,
    pub start: u32,
    pub count: u32,
    pub min: Vec<f32>,
    pub max: Vec<f32>,
}

/// One `engine_feed` message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FeedFrame {
    pub seq: u64,
    /// The first frame after a subscribe or a new engine: the UI replaces its state with this one. It
    /// carries every lane, the transport and the selected lane (a lane or transport a new engine has
    /// not reported yet is at its default: EMPTY, no master).
    pub reset: bool,
    pub events: Vec<WireEvent>,
    pub device: Vec<DeviceEvent>,
    /// Absent: unchanged; `null`: no device runs; else the device that runs.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "present")]
    pub status: Option<Option<DeviceStatus>>,
    /// `null` while no device runs.
    pub anchor: Option<ClockAnchor>,
    /// `null` while no device runs.
    pub meter: Option<Meter>,
    pub peaks: Vec<PeakUpdate>,
    /// Reset frames only (absent otherwise): the settings the host keeps and replays into every new
    /// engine (`settings.rs`), in replay order and as the UI sends them. A setting never sent, or one a
    /// lane's clear forgot, is at the engine's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<Vec<WireCommand>>,
}

/// A lane's mix as a snapshot's track carries it, in session.json's track shape
/// (`src/session/session-schema.ts`): `{"volume","muted","dubFeedback","fx"}`, `fx` the five effects in
/// chain order, each `{"bypassed","params"}` with its params by their TS keys (`validateFxStates` reads
/// the same). A missing or unknown key, or a value that is no number, is refused.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "MixJson", into = "MixJson")]
pub struct WireLaneMix(pub LaneMix);

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MixJson {
    volume: f32,
    muted: bool,
    dub_feedback: f32,
    fx: Vec<FxJson>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FxJson {
    bypassed: bool,
    params: BTreeMap<String, f64>,
}

impl From<WireLaneMix> for MixJson {
    fn from(WireLaneMix(m): WireLaneMix) -> MixJson {
        let fx = FxKind::ALL
            .iter()
            .map(|kind| {
                let state = m.fx[kind.index()];
                let params = kind.params().iter().zip(state.params).map(|(def, v)| (def.key.to_string(), v)).collect();
                FxJson { bypassed: state.bypassed, params }
            })
            .collect();
        MixJson { volume: m.volume, muted: m.muted, dub_feedback: m.dub_feedback, fx }
    }
}

impl TryFrom<MixJson> for WireLaneMix {
    type Error = String;

    fn try_from(j: MixJson) -> Result<WireLaneMix, String> {
        if !j.volume.is_finite() || !j.dub_feedback.is_finite() {
            return Err("a mix's volume and dubFeedback are numbers".to_string());
        }
        if j.fx.len() != FxKind::ALL.len() {
            return Err(format!("a mix holds {} effects, not {}", j.fx.len(), FxKind::ALL.len()));
        }
        let mut fx = [FxState { bypassed: true, params: [0.0; MAX_PARAMS] }; 5];
        for (kind, entry) in FxKind::ALL.into_iter().zip(j.fx) {
            let defs = kind.params();
            if entry.params.len() != defs.len() {
                return Err(format!("the {kind:?} effect's params are not {}", defs.len()));
            }
            let state = &mut fx[kind.index()];
            state.bypassed = entry.bypassed;
            for (slot, def) in state.params.iter_mut().zip(defs) {
                match entry.params.get(def.key) {
                    Some(v) if v.is_finite() => *slot = *v,
                    _ => return Err(format!("the {kind:?} effect's \"{}\" is missing or no number", def.key)),
                }
            }
        }
        Ok(WireLaneMix(LaneMix { volume: j.volume, muted: j.muted, dub_feedback: j.dub_feedback, fx }))
    }
}

/// A field that may be absent (`None`), `null` (`Some(None)`) or a value.
mod present {
    use super::*;

    pub fn serialize<S: Serializer, T: Serialize>(v: &Option<Option<T>>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(inner) => inner.serialize(s),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
        Option::<T>::deserialize(d).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{DeviceRequest, OpenError};
    use super::*;
    use serde_json::Value;

    const FIXTURE: &str = include_str!("../../../verify/fixtures/engine-wire.json");

    /// Every `Command` variant, by position: a new variant fails to compile here until it has a
    /// number (bump `COMMANDS`) and an example in the fixture.
    const COMMANDS: usize = 43;
    fn command_index(c: &Command) -> usize {
        use Command::*;
        match c {
            RecDub(_) => 0,
            PlayStop(_) => 1,
            Stop(_) => 2,
            Undo(_) => 3,
            Reverse(_) => 4,
            Copy(_) => 5,
            Clear(_) => 6,
            PlayAll => 7,
            StopAll => 8,
            ClearAll => 9,
            Action(_) => 10,
            ActionOn(..) => 11,
            SelectTrack(_) => 12,
            SetBpm(_) => 13,
            SetMetronome(_) => 14,
            SetClickVolume(_) => 15,
            SetMasterVolume(_) => 16,
            SetMasterMute(_) => 17,
            SetLoopEndStop(_) => 18,
            SetFixedLength(_) => 19,
            SetFixedBars(_) => 20,
            SetRetake(_) => 21,
            SetAutoRecord(_) => 22,
            SetAutoSensitivity(_) => 23,
            SetVolume(..) => 24,
            SetMute(..) => 25,
            SetFxParam(..) => 26,
            SetFxBypass(..) => 27,
            SelectInstrument(_) => 28,
            NoteOn(..) => 29,
            NoteOff(_) => 30,
            PitchBend(_) => 31,
            Modulation(_) => 32,
            AllNotesOff => 33,
            SetSlotLive(..) => 34,
            SetSlotGain(..) => 35,
            SetInputSend(..) => 36,
            SetInputSendParam(..) => 37,
            Trim(..) => 38,
            Press => 39,
            SetDubFeedback(..) => 40,
            SetFadeBars(_) => 41,
            SetInstrumentGain(..) => 42,
        }
    }

    /// Every `Action` and `Refusal` variant, by position: a new one fails to compile here until the
    /// fixture sends (or answers) it.
    const ACTIONS: usize = 15;
    fn action_index(a: &Action) -> usize {
        match a {
            Action::RecDub => 0,
            Action::PlayStop => 1,
            Action::Undo => 2,
            Action::Clear => 3,
            Action::NextTrack => 4,
            Action::PrevTrack => 5,
            Action::PlayAll => 6,
            Action::StopAll => 7,
            Action::Mute => 8,
            Action::Reverse => 9,
            Action::Copy => 10,
            Action::Halve => 11,
            Action::Hold(_) => 12,
            Action::Release(_) => 13,
            Action::FadeAll => 14,
        }
    }

    const REFUSALS: usize = 16;
    fn refusal_index(r: &Refusal) -> usize {
        match r {
            Refusal::Stopping => 0,
            Refusal::PlayFirst => 1,
            Refusal::Reversed => 2,
            Refusal::OtherRecording => 3,
            Refusal::Empty => 4,
            Refusal::NoUndo => 5,
            Refusal::NoClear => 6,
            Refusal::ConfirmClear => 7,
            Refusal::Capturing => 8,
            Refusal::NoTrim => 9,
            Refusal::NoMute => 10,
            Refusal::NoReverse => 11,
            Refusal::NoCopy => 12,
            Refusal::NoFreeLane => 13,
            Refusal::Fading => 14,
            Refusal::NoFade => 15,
        }
    }

    const EVENTS: usize = 10;
    fn event_index(e: &Event) -> usize {
        match e {
            Event::Lane { .. } => 0,
            Event::Transport { .. } => 1,
            Event::Beat { .. } => 2,
            Event::Selected { .. } => 3,
            Event::Refused { .. } => 4,
            Event::TakeRejected { .. } => 5,
            Event::PassDropped { .. } => 6,
            Event::Copied { .. } => 7,
            Event::Cleared { .. } => 8,
            Event::Muted { .. } => 9,
        }
    }

    const DEVICE_EVENTS: usize = 6;
    fn device_event_index(e: &DeviceEvent) -> usize {
        match e {
            DeviceEvent::Lost { .. } => 0,
            DeviceEvent::Recovered(_) => 1,
            DeviceEvent::Fallback(_) => 2,
            DeviceEvent::ShareLost { .. } => 3,
            DeviceEvent::EngineFaulted => 4,
            DeviceEvent::LoopsDropped { .. } => 5,
        }
    }

    fn fixture() -> Value {
        serde_json::from_str(FIXTURE).expect("the fixture is JSON")
    }

    /// The same JSON, a number compared by its value (JSON has one number type: `4` is `4.0`).
    fn same(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
            (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same(x, y)),
            (Value::Object(x), Value::Object(y)) => x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same(v, w))),
            _ => a == b,
        }
    }

    /// Parse every entry of `section` as `T` (as a Tauri argument arrives: from a `Value`) and write it
    /// back as text (as the IPC sends it): the same JSON, entry by entry.
    fn round_trip<T: Serialize + for<'de> Deserialize<'de>>(section: &str) -> Vec<T> {
        let entries = fixture()[section].as_array().unwrap_or_else(|| panic!("fixture.{section} is an array")).clone();
        entries
            .into_iter()
            .map(|entry| {
                let parsed: T = serde_json::from_value(entry.clone()).unwrap_or_else(|e| panic!("fixture.{section}: {entry}: {e}"));
                let text = serde_json::to_string(&parsed).unwrap();
                let back: Value = serde_json::from_str(&text).unwrap();
                assert!(same(&back, &entry), "fixture.{section}: {entry} writes back as {text}");
                parsed
            })
            .collect()
    }

    fn covers(section: &str, indices: impl Iterator<Item = usize>, total: usize) {
        let mut seen = vec![false; total];
        for k in indices {
            seen[k] = true;
        }
        let missing: Vec<usize> = (0..total).filter(|&k| !seen[k]).collect();
        assert!(missing.is_empty(), "fixture.{section} lacks the variants at {missing:?}");
    }

    #[test]
    fn every_command_round_trips_through_the_fixture() {
        let commands: Vec<WireCommand> = round_trip("commands");
        covers("commands", commands.iter().map(|c| command_index(&c.0)), COMMANDS);
        let actions = commands.iter().filter_map(|c| match c.0 {
            Command::Action(a) | Command::ActionOn(_, a) => Some(action_index(&a)),
            _ => None,
        });
        covers("commands (their actions)", actions, ACTIONS);
        let targets = commands.iter().filter_map(|c| match c.0 {
            Command::SelectInstrument(NoteTarget::Builtin(_)) => Some(0),
            Command::SelectInstrument(NoteTarget::Slot(_)) => Some(1),
            Command::SelectInstrument(NoteTarget::Off) => Some(2),
            _ => None,
        });
        covers("commands (their note targets)", targets, 3);
    }

    #[test]
    fn every_event_round_trips_through_the_fixture() {
        let events: Vec<WireEvent> = round_trip("events");
        covers("events", events.iter().map(|e| event_index(&e.0)), EVENTS);
        let refusals = events.iter().filter_map(|e| match e.0 {
            Event::Refused { reason, .. } => Some(refusal_index(&reason)),
            _ => None,
        });
        covers("events (their refusals)", refusals, REFUSALS);
    }

    #[test]
    fn every_device_event_round_trips_through_the_fixture() {
        let events: Vec<DeviceEvent> = round_trip("deviceEvents");
        covers("deviceEvents", events.iter().map(device_event_index), DEVICE_EVENTS);
    }

    #[test]
    fn an_open_error_is_its_text_and_a_refusal_an_object() {
        let errors: Vec<OpenError> = round_trip("openErrors");
        assert!(errors.iter().any(|e| matches!(e, OpenError::RateChange { .. })), "a refusal");
        assert!(errors.iter().any(|e| matches!(e, OpenError::Failed(_))), "a failure");
        let failed = serde_json::to_value(OpenError::Failed("the device did not start".into())).unwrap();
        assert_eq!(failed, Value::from("the device did not start"), "a failure stays a plain string");
    }

    #[test]
    fn requests_statuses_and_feed_frames_round_trip_through_the_fixture() {
        let requests: Vec<DeviceRequest> = round_trip("deviceRequests");
        let picks = requests.iter().flat_map(|r| r.input_channels);
        assert!(picks.clone().any(|c| c.is_none()) && picks.clone().any(|c| c.is_some()), "auto and picked channels");
        assert!(requests.iter().any(|r| r.input_channels[0] != r.input_channels[1]), "each slot its own");
        assert!(requests.iter().any(|r| r.buffer.is_some()), "null and set fields");
        assert!(requests.iter().any(|r| r.sample_rate.is_some()) && requests.iter().any(|r| r.sample_rate.is_none()), "a rate pick and none");
        let one: DeviceRequest = serde_json::from_str(r#"{"backend":"Asio","input":null,"output":null,"inputChannel":3,"buffer":null}"#).unwrap();
        assert_eq!(one.input_channels, [Some(3), Some(3)], "one inputChannel sets both slots");
        let auto: DeviceRequest = serde_json::from_str(r#"{"backend":"Wasapi","input":null,"output":null,"inputChannel":null,"buffer":null}"#).unwrap();
        assert_eq!(auto.input_channels, [None, None]);
        assert_eq!(auto.sample_rate, None, "a request from before the rate pick asks for the device's own");
        let _: Vec<DeviceStatus> = round_trip("deviceStatuses");
        let frames: Vec<FeedFrame> = round_trip("feed");
        assert!(frames.iter().any(|f| f.reset && matches!(f.status, Some(Some(_)))), "a reset frame with a status");
        assert!(frames.iter().any(|f| f.status == Some(None)), "a frame whose device stopped (status null)");
        assert!(frames.iter().any(|f| f.status.is_none()), "a frame with no status change (status absent)");
        assert!(frames.iter().all(|f| f.settings.is_some() == f.reset), "settings on the reset frames only");
    }

    #[test]
    fn a_snapshot_header_round_trips_through_the_fixture_with_each_tracks_mix() {
        let _: Vec<super::super::session::SnapshotHeader> = round_trip("snapshotHeaders");
        let entry = &fixture()["snapshotHeaders"][0]["tracks"][0]["mix"];
        let WireLaneMix(mix) = serde_json::from_value(entry.clone()).unwrap();
        assert_eq!((mix.volume, mix.muted, mix.dub_feedback), (0.5, true, 0.25));
        assert_eq!(mix.fx[FxKind::Delay.index()], FxState { bypassed: false, params: [2.0, 0.95, 0.75] }, "the params by their keys, in def order");
        let louder = serde_json::to_value(WireLaneMix(LaneMix { volume: 1.5, ..LaneMix::default() })).unwrap();
        assert!(same(&louder, &fixture()["snapshotHeaders"][0]["tracks"][1]["mix"]), "the engine's defaults write as the fixture's: {louder}");
        let refused = |edit: fn(&mut Value)| {
            let mut bad = entry.clone();
            edit(&mut bad);
            serde_json::from_value::<WireLaneMix>(bad).is_err()
        };
        assert!(refused(|m| m["fx"].as_array_mut().unwrap().truncate(4)), "five effects");
        assert!(refused(|m| _ = m["fx"][0]["params"].as_object_mut().unwrap().remove("q")), "a missing param");
        assert!(refused(|m| m["fx"][0]["params"]["Q"] = Value::from(2)), "an unknown param");
        assert!(refused(|m| m["fx"][1]["params"]["semitones"] = Value::from("-5")), "a param that is no number");
        assert!(refused(|m| m["dub_feedback"] = Value::from(1)), "camelCase fields");
    }

    #[test]
    fn a_load_header_round_trips_through_the_fixture_and_a_track_without_its_mix_is_refused() {
        let _: Vec<super::super::session::LoadHeader> = round_trip("loadHeaders");
        let entry = &fixture()["loadHeaders"][0];
        let WireLaneMix(mix) = serde_json::from_value(entry["tracks"][0]["mix"].clone()).unwrap();
        assert_eq!((mix.volume, mix.muted, mix.dub_feedback), (0.25, true, 0.5));
        assert_eq!(mix.fx[FxKind::Reverb.index()], FxState { bypassed: false, params: [0.75, 0.0, 0.0] });
        let mut bare = entry.clone();
        bare["tracks"][1].as_object_mut().unwrap().remove("mix");
        assert!(serde_json::from_value::<super::super::session::LoadHeader>(bare).is_err(), "every load track carries its mix");
    }

    #[test]
    fn the_wire_names_match_the_engine() {
        let command = |json: &str| serde_json::from_str::<WireCommand>(json).map(|c| c.0);
        assert_eq!(command(r#"{"SetFxParam":[2,"feedback",0.5]}"#).unwrap(), Command::SetFxParam(2, FxParam::Feedback, 0.5));
        assert_eq!(command(r#"{"SelectInstrument":{"Builtin":"drum"}}"#).unwrap(), Command::SelectInstrument(NoteTarget::Builtin(Instrument::Drums)));
        assert!(command(r#"{"SetFxParam":[2,"Feedback",0.5]}"#).is_err(), "keys are the TS keys, lower case");
        assert!(command(r#"{"SelectInstrument":{"Builtin":"drums"}}"#).is_err(), "an instrument is its id");
        assert_eq!(command(r#"{"SetInputSend":["echo",true]}"#).unwrap(), Command::SetInputSend(InputSend::Echo, true));
        assert!(command(r#"{"SetInputSend":["Echo",true]}"#).is_err(), "a send is its key");
        assert_eq!(command(r#"{"SetInputSend":["ring",true]}"#).unwrap(), Command::SetInputSend(InputSend::Ring, true));
        assert!(command(r#"{"SetInputSend":["ringMod",true]}"#).is_err(), "the ring's key is \"ring\"");
        assert_eq!(
            command(r#"{"SetInputSendParam":["ringFreq",440]}"#).unwrap(),
            Command::SetInputSendParam(InputSendParam::RingFreq, 440.0)
        );
        assert_eq!(
            command(r#"{"SetInputSendParam":["ringLevel",0.5]}"#).unwrap(),
            Command::SetInputSendParam(InputSendParam::RingLevel, 0.5)
        );
        for send in InputSend::ALL {
            let json = serde_json::to_value(WireCommand(Command::SetInputSend(send, true))).unwrap();
            assert_eq!(json["SetInputSend"][0], Value::from(send.key()));
        }
        assert_eq!(command(r#"{"Trim":[1,3]}"#).unwrap(), Command::Trim(1, 3));
        assert!(command(r#"{"Trim":[1,-1]}"#).is_err() && command(r#"{"Trim":[1,1.5]}"#).is_err(), "a bar count is a whole number");
        assert_eq!(command(r#"{"SetFadeBars":4}"#).unwrap(), Command::SetFadeBars(4));
        assert!(command(r#"{"SetFadeBars":2.5}"#).is_err(), "a fade's bars are a whole number");
        assert_eq!(command(r#"{"Action":"FadeAll"}"#).unwrap(), Command::Action(Action::FadeAll));
        assert_eq!(command(r#"{"SetDubFeedback":[2,0.5]}"#).unwrap(), Command::SetDubFeedback(2, 0.5));
        assert_eq!(command(r#"{"SelectInstrument":"Off"}"#).unwrap(), Command::SelectInstrument(NoteTarget::Off));
        assert_eq!(command(r#"{"SetInstrumentGain":["bass",0.5]}"#).unwrap(), Command::SetInstrumentGain(Instrument::Bass, 0.5));
        assert!(command(r#"{"SetInstrumentGain":["Bass",0.5]}"#).is_err(), "an instrument is its id");
        for param in InputSendParam::ALL {
            let json = serde_json::to_value(WireCommand(Command::SetInputSendParam(param, param.range().2))).unwrap();
            assert_eq!(json["SetInputSendParam"][0], Value::from(param.key()));
        }
        for kind in FxKind::ALL {
            let key = serde_json::to_value(WireCommand(Command::SetFxBypass(0, kind, true))).unwrap()["SetFxBypass"][1].clone();
            assert_eq!(key, Value::from(kind.label().to_ascii_lowercase()), "{kind:?}");
            for def in kind.params() {
                let param = FxParam::from_key(kind, def.key).unwrap();
                let json = serde_json::to_value(WireCommand(Command::SetFxParam(0, param, def.default))).unwrap();
                assert_eq!(json["SetFxParam"][1], Value::from(def.key));
            }
        }
    }
}
