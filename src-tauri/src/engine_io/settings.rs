//! OWNS: the last value of every setting command the engine took (or the UI sent before the first open),
//! replayed into each new engine (the first, one at another sample rate, one that replaced a faulted
//! engine), so the UI's settings survive a rebuild and a setting sent before the first open is kept. A
//! setting the full command ring refused is not kept (`EngineHost::send`).
//!
//! A setting is a command that sets a value (the tempo, the click, the master, the input sends, the
//! looper's modes and FADE's length, a lane's volume, mute, DUB FEEDBACK and FX, the note target, the
//! wheels, a built-in instrument's level, a plugin slot's live flag and gain, the selected lane);
//! everything else (the looper's gestures, notes) acts once and is not kept. What the engine resets, the memory forgets, as the feed reads it
//! happen: a cleared lane's volume, mute, DUB FEEDBACK and FX (`cleared`, on the engine's `Cleared`
//! event: CLEAR, a pedal's CLEAR, CLEAR ALL), and a COPY hands the
//! destination the source's (`copy_lane`, on `Copied`, whose DUB FEEDBACK is the value the engine copied,
//! not the source's as the memory holds it by then). A setting for a lane sent in the moment between
//! its clear and the feed reading it (a block and a feed tick) is forgotten with it. What the engine sets
//! itself, the memory keeps as if the UI had sent it: a pedal's MUTE (`Muted`, as `SetMute`), and a
//! loaded lane's mix (`loaded`, once the load answers: `session.rs`).

use std::collections::BTreeMap;

use lf_engine::dsp::fx::{FxKind, FxParam, MAX_PARAMS};
use lf_engine::{Command, LaneMix, SLOT_COUNT, TRACK_COUNT};

/// What a setting sets; replayed in this order (the note target before the wheels it hands over).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Bpm,
    Metronome,
    ClickVolume,
    MasterVolume,
    MasterMute,
    /// An input send's param, by `InputSendParam as usize`; before the sends' on/off, so a send that
    /// comes on in a new engine comes on with its values.
    InputSendParam(usize),
    /// An input send, by `InputSend as usize`.
    InputSend(usize),
    LoopEndStop,
    FadeBars,
    FixedLength,
    FixedBars,
    Retake,
    AutoRecord,
    AutoSensitivity,
    SelectTrack,
    Volume(u8),
    Mute(u8),
    DubFeedback(u8),
    /// A lane's effect, by `FxKind::index`.
    FxBypass(u8, usize),
    /// A lane's FX param, by `FxKind::index * MAX_PARAMS + FxParam::index`.
    FxParam(u8, usize),
    /// A built-in instrument's level, by `Instrument as usize`.
    InstrumentGain(usize),
    Instrument,
    PitchBend,
    Modulation,
    SlotLive(u8),
    SlotGain(u8),
}

impl Key {
    fn lane(self) -> Option<u8> {
        match self {
            Key::Volume(i) | Key::Mute(i) | Key::DubFeedback(i) | Key::FxBypass(i, _) | Key::FxParam(i, _) => Some(i),
            _ => None,
        }
    }
}

/// The key a setting command sets; `None` for an action (or a lane or slot out of range).
fn key(command: &Command) -> Option<Key> {
    let lane = |i: u8| (usize::from(i) < TRACK_COUNT).then_some(i);
    let slot = |i: u8| (usize::from(i) < SLOT_COUNT).then_some(i);
    Some(match *command {
        Command::SetBpm(_) => Key::Bpm,
        Command::SetMetronome(_) => Key::Metronome,
        Command::SetClickVolume(_) => Key::ClickVolume,
        Command::SetMasterVolume(_) => Key::MasterVolume,
        Command::SetMasterMute(_) => Key::MasterMute,
        Command::SetInputSendParam(param, _) => Key::InputSendParam(param as usize),
        Command::SetInputSend(send, _) => Key::InputSend(send as usize),
        Command::SetLoopEndStop(_) => Key::LoopEndStop,
        Command::SetFadeBars(_) => Key::FadeBars,
        Command::SetFixedLength(_) => Key::FixedLength,
        Command::SetFixedBars(_) => Key::FixedBars,
        Command::SetRetake(_) => Key::Retake,
        Command::SetAutoRecord(_) => Key::AutoRecord,
        Command::SetAutoSensitivity(_) => Key::AutoSensitivity,
        Command::SelectTrack(i) => lane(i).map(|_| Key::SelectTrack)?,
        Command::SetVolume(i, _) => Key::Volume(lane(i)?),
        Command::SetMute(i, _) => Key::Mute(lane(i)?),
        Command::SetDubFeedback(i, _) => Key::DubFeedback(lane(i)?),
        Command::SetFxBypass(i, kind, _) => Key::FxBypass(lane(i)?, kind.index()),
        Command::SetFxParam(i, param, _) => Key::FxParam(lane(i)?, param.kind().index() * MAX_PARAMS + param.index()),
        Command::SetInstrumentGain(i, _) => Key::InstrumentGain(i as usize),
        Command::SelectInstrument(_) => Key::Instrument,
        Command::PitchBend(_) => Key::PitchBend,
        Command::Modulation(_) => Key::Modulation,
        Command::SetSlotLive(i, _) => Key::SlotLive(slot(i)?),
        Command::SetSlotGain(i, _) => Key::SlotGain(slot(i)?),
        Command::RecDub(_)
        | Command::PlayStop(_)
        | Command::Stop(_)
        | Command::Undo(_)
        | Command::Reverse(_)
        | Command::Copy(_)
        | Command::Trim(..)
        | Command::Clear(_)
        | Command::PlayAll
        | Command::StopAll
        | Command::ClearAll
        | Command::Action(_)
        | Command::ActionOn(..)
        | Command::Press
        | Command::NoteOn(..)
        | Command::NoteOff(_)
        | Command::AllNotesOff => return None,
    })
}

/// `command`, moved to lane `to`.
fn on_lane(command: Command, to: u8) -> Command {
    match command {
        Command::SetVolume(_, v) => Command::SetVolume(to, v),
        Command::SetMute(_, m) => Command::SetMute(to, m),
        Command::SetDubFeedback(_, v) => Command::SetDubFeedback(to, v),
        Command::SetFxBypass(_, kind, b) => Command::SetFxBypass(to, kind, b),
        Command::SetFxParam(_, param, v) => Command::SetFxParam(to, param, v),
        other => other,
    }
}

#[derive(Default)]
pub(crate) struct Settings {
    last: BTreeMap<Key, Command>,
}

impl Settings {
    /// Keep `command` if it is a setting (true).
    pub(crate) fn record(&mut self, command: &Command) -> bool {
        match key(command) {
            Some(key) => {
                self.last.insert(key, *command);
                true
            }
            None => false,
        }
    }

    /// Lane `to` took lane `from`'s mixer and FX, and the DUB FEEDBACK `feedback` (the engine's COPY,
    /// whose `Copied` says what it copied: the source's may have moved before the copy was done).
    pub(crate) fn copy_lane(&mut self, from: u8, to: u8, feedback: f32) {
        if from == to {
            return;
        }
        self.forget_lane(to);
        let copied: Vec<(Key, Command)> = self
            .last
            .iter()
            .filter(|(k, _)| k.lane() == Some(from))
            .map(|(k, c)| {
                let key = match *k {
                    Key::Volume(_) => Key::Volume(to),
                    Key::Mute(_) => Key::Mute(to),
                    Key::DubFeedback(_) => Key::DubFeedback(to),
                    Key::FxBypass(_, x) => Key::FxBypass(to, x),
                    Key::FxParam(_, x) => Key::FxParam(to, x),
                    other => other,
                };
                (key, on_lane(*c, to))
            })
            .collect();
        self.last.extend(copied);
        self.record(&Command::SetDubFeedback(to, feedback));
    }

    /// A load set lane `lane`'s mix: kept as the commands that set it, replacing what the lane had.
    pub(crate) fn loaded(&mut self, lane: u8, mix: &LaneMix) {
        self.forget_lane(lane);
        self.record(&Command::SetVolume(lane, mix.volume));
        self.record(&Command::SetMute(lane, mix.muted));
        self.record(&Command::SetDubFeedback(lane, mix.dub_feedback));
        for kind in FxKind::ALL {
            let state = &mix.fx[kind.index()];
            for (def, &value) in kind.params().iter().zip(&state.params) {
                if let Some(param) = FxParam::from_key(kind, def.key) {
                    self.record(&Command::SetFxParam(lane, param, value));
                }
            }
            self.record(&Command::SetFxBypass(lane, kind, state.bypassed));
        }
    }

    /// The engine cleared lane `lane`: its volume, mute, DUB FEEDBACK and FX are back at their defaults.
    pub(crate) fn cleared(&mut self, lane: u8) {
        self.forget_lane(lane);
    }

    fn forget_lane(&mut self, lane: u8) {
        self.last.retain(|k, _| k.lane() != Some(lane));
    }

    /// Every kept setting, in replay order.
    pub(crate) fn replay(&self) -> impl Iterator<Item = Command> + '_ {
        self.last.values().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lf_engine::dsp::fx::{FxKind, FxParam};
    use lf_engine::{InputSend, InputSendParam, Instrument, NoteTarget};

    fn replay(s: &Settings) -> Vec<Command> {
        s.replay().collect()
    }

    #[test]
    fn the_last_value_of_each_setting_is_kept_and_actions_are_not() {
        let mut s = Settings::default();
        assert!(s.record(&Command::SetBpm(90.0)));
        assert!(s.record(&Command::SetBpm(100.0)));
        assert!(!s.record(&Command::RecDub(0)));
        assert!(!s.record(&Command::NoteOn(60, 1.0)));
        assert!(s.record(&Command::PitchBend(1.0)));
        assert!(s.record(&Command::SelectInstrument(NoteTarget::Builtin(Instrument::Pad))));
        assert!(!s.record(&Command::SetVolume(9, 0.5)), "a lane out of range is not a setting");
        assert!(!s.record(&Command::SetSlotLive(2, true)), "a slot out of range is not a setting");
        assert!(s.record(&Command::SetInputSend(InputSend::Echo, true)));
        assert!(s.record(&Command::SetInputSendParam(InputSendParam::EchoLevel, 0.2)));
        assert!(s.record(&Command::SetInputSendParam(InputSendParam::EchoLevel, 0.6)));
        assert!(s.record(&Command::SetFadeBars(4)));
        assert!(s.record(&Command::SetInstrumentGain(Instrument::Bass, 0.3)));
        assert!(s.record(&Command::SetInstrumentGain(Instrument::Lead, 0.5)));
        assert!(s.record(&Command::SetInstrumentGain(Instrument::Bass, 0.4)));
        assert_eq!(
            replay(&s),
            [
                Command::SetBpm(100.0),
                Command::SetInputSendParam(InputSendParam::EchoLevel, 0.6),
                Command::SetInputSend(InputSend::Echo, true),
                Command::SetFadeBars(4),
                Command::SetInstrumentGain(Instrument::Lead, 0.5),
                Command::SetInstrumentGain(Instrument::Bass, 0.4),
                Command::SelectInstrument(NoteTarget::Builtin(Instrument::Pad)),
                Command::PitchBend(1.0)
            ],
            "the note target is replayed before the wheels it hands over, a send's values before the send"
        );
    }

    #[test]
    fn a_cleared_lane_forgets_what_the_engine_resets_and_copy_hands_it_on() {
        let mut s = Settings::default();
        s.record(&Command::SetVolume(0, 0.5));
        s.record(&Command::SetFxParam(0, FxParam::Cutoff, 900.0));
        s.record(&Command::SetFxBypass(0, FxKind::Delay, false));
        s.record(&Command::SetMute(1, true));
        s.record(&Command::SetDubFeedback(0, 0.25));
        s.record(&Command::SetMasterVolume(0.7));
        s.record(&Command::SetInputSend(InputSend::Reverb, true));
        s.copy_lane(0, 3, 0.25);
        assert!(replay(&s).contains(&Command::SetFxParam(3, FxParam::Cutoff, 900.0)));
        assert!(replay(&s).contains(&Command::SetVolume(3, 0.5)));
        assert!(replay(&s).contains(&Command::SetDubFeedback(3, 0.25)), "COPY hands DUB FEEDBACK on");
        // The source moved on before the copy was done: the copy keeps the value the engine copied.
        s.record(&Command::SetDubFeedback(0, 0.75));
        s.copy_lane(0, 4, 0.25);
        assert!(replay(&s).contains(&Command::SetDubFeedback(4, 0.25)), "the copied value, not the source's since");
        assert!(replay(&s).contains(&Command::SetDubFeedback(0, 0.75)));
        assert!(!s.record(&Command::Clear(0)), "a CLEAR is an action: the engine's Cleared event is what forgets");
        assert!(replay(&s).contains(&Command::SetVolume(0, 0.5)));
        s.cleared(0);
        assert!(!replay(&s).iter().any(|c| matches!(c, Command::SetVolume(0, _) | Command::SetDubFeedback(0, _) | Command::SetFxParam(0, ..) | Command::SetFxBypass(0, ..))));
        assert!(replay(&s).contains(&Command::SetMute(1, true)));
        (0..TRACK_COUNT as u8).for_each(|i| s.cleared(i));
        assert_eq!(
            replay(&s),
            [Command::SetMasterVolume(0.7), Command::SetInputSend(InputSend::Reverb, true)],
            "CLEAR ALL clears every lane, not the master nor the input sends"
        );
    }
}
