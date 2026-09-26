//! OWNS: the JSON wire between the UI and the engine host (`docs/plans/native-engine.md` § Stage 5,
//! Wire): the serde mirror of lf-engine's commands and events (the crate stays serde-free), the feed
//! frame the feed thread sends, and the fixture test. The TS mirror is `src/platform/engine-wire.ts`;
//! `verify/fixtures/engine-wire.json` holds both sides to the same JSON (this file's test and a TS
//! guard parse it).
//!
//! Serde's external tagging with the Rust variant names: a unit variant is its name (`"PlayAll"`), a
//! newtype `{"RecDub":0}`, a tuple `{"SetVolume":[0,0.8]}`, a struct variant an object with camelCase
//! fields. `FxParam`/`FxKind` travel as the TS keys (`src/audio/fx/metadata.ts`), an `Instrument` as its
//! id, a `NoteTarget` as `{"Builtin":"lead"}` / `{"Slot":0}`, a `Frame` (i64) as a JSON number. The
//! mirrors are serde `remote` derives: a variant or field lf-engine adds fails to compile here until it
//! is mirrored.
//!
//! `DeviceRequest`, `DeviceStatus`, `DeviceEvent` and `AudioBackend` derive serde where they are
//! defined (camelCase fields, backends as `"Asio"` / `"Wasapi"`).

use lf_engine::dsp::fx::{FxKind, FxParam};
use lf_engine::grid::Frame;
use lf_engine::{Action, Command, Event, Instrument, LaneInfo, LaneState, NoteTarget, Refusal};
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
    Clear(u8),
    PlayAll,
    StopAll,
    ClearAll,
    Action(#[serde(with = "ActionDef")] Action),
    ActionOn(u8, #[serde(with = "ActionDef")] Action),
    SelectTrack(u8),
    SetBpm(f64),
    SetMetronome(bool),
    SetClickVolume(f32),
    SetMasterVolume(f32),
    SetMasterMute(bool),
    SetLoopEndStop(bool),
    SetFixedLength(bool),
    SetFixedBars(f64),
    SetRetake(bool),
    SetAutoRecord(bool),
    SetAutoSensitivity(f64),
    SetVolume(u8, f32),
    SetMute(u8, bool),
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
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "NoteTarget")]
enum NoteTargetDef {
    Builtin(#[serde(with = "instrument")] Instrument),
    Slot(u8),
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
    Copied { frame: Frame, from: u8, to: u8 },
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

/// The TS `FxKind` keys, in `FxKind::ALL` order (`FX_META` in `src/audio/fx/metadata.ts`).
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

/// The device input (the capture channel the engine takes, before any plugin) since the last frame:
/// its linear peak, and whether a sample reached full scale.
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
    use super::super::DeviceRequest;
    use super::*;
    use serde_json::Value;

    const FIXTURE: &str = include_str!("../../../verify/fixtures/engine-wire.json");

    /// Every `Command` variant, by position: a new variant fails to compile here until it has a
    /// number (bump `COMMANDS`) and an example in the fixture.
    const COMMANDS: usize = 36;
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
        }
    }

    const EVENTS: usize = 8;
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
        }
    }

    const DEVICE_EVENTS: usize = 5;
    fn device_event_index(e: &DeviceEvent) -> usize {
        match e {
            DeviceEvent::Lost { .. } => 0,
            DeviceEvent::Recovered(_) => 1,
            DeviceEvent::Fallback(_) => 2,
            DeviceEvent::ShareLost { .. } => 3,
            DeviceEvent::EngineFaulted => 4,
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
    }

    #[test]
    fn every_event_round_trips_through_the_fixture() {
        let events: Vec<WireEvent> = round_trip("events");
        covers("events", events.iter().map(|e| event_index(&e.0)), EVENTS);
    }

    #[test]
    fn every_device_event_round_trips_through_the_fixture() {
        let events: Vec<DeviceEvent> = round_trip("deviceEvents");
        covers("deviceEvents", events.iter().map(device_event_index), DEVICE_EVENTS);
    }

    #[test]
    fn requests_statuses_and_feed_frames_round_trip_through_the_fixture() {
        let requests: Vec<DeviceRequest> = round_trip("deviceRequests");
        assert!(requests.iter().any(|r| r.input_channel.is_none()) && requests.iter().any(|r| r.buffer.is_some()), "null and set fields");
        let _: Vec<DeviceStatus> = round_trip("deviceStatuses");
        let frames: Vec<FeedFrame> = round_trip("feed");
        assert!(frames.iter().any(|f| f.reset && matches!(f.status, Some(Some(_)))), "a reset frame with a status");
        assert!(frames.iter().any(|f| f.status == Some(None)), "a frame whose device stopped (status null)");
        assert!(frames.iter().any(|f| f.status.is_none()), "a frame with no status change (status absent)");
    }

    #[test]
    fn the_wire_names_match_the_engine() {
        let command = |json: &str| serde_json::from_str::<WireCommand>(json).map(|c| c.0);
        assert_eq!(command(r#"{"SetFxParam":[2,"feedback",0.5]}"#).unwrap(), Command::SetFxParam(2, FxParam::Feedback, 0.5));
        assert_eq!(command(r#"{"SelectInstrument":{"Builtin":"drum"}}"#).unwrap(), Command::SelectInstrument(NoteTarget::Builtin(Instrument::Drums)));
        assert!(command(r#"{"SetFxParam":[2,"Feedback",0.5]}"#).is_err(), "keys are the TS keys, lower case");
        assert!(command(r#"{"SelectInstrument":{"Builtin":"drums"}}"#).is_err(), "an instrument is its id");
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
